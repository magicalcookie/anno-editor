// Components on widgets (ComponentInstance + PropertyValue overrides) and a
// pure evaluator for the layout components the game applies at runtime:
// StackLayout with its Padding/Alignment utilities, BackgroundLayout and
// ProportionalPivotPoint. Semantics come from the native component code;
// the evaluator is fed a caller-built tree so it can span files
// (scene → container → template).

use std::{collections::HashMap, sync::Arc};

/// A hash map for the layout's own keys (node indices, instance keys,
/// guids): foldhash, several times cheaper than SipHash on them.
pub(super) type FastMap<K, V> = HashMap<K, V, foldhash::fast::RandomState>;

/// The hasher for fingerprints and keys that must hold across builds and
/// layouts within the process: foldhash with a fixed seed. Our own data,
/// so not SipHash's resistance to chosen collisions.
pub(super) fn hasher() -> impl std::hash::Hasher {
    std::hash::BuildHasher::build_hasher(&foldhash::fast::FixedState::with_seed(0))
}

use super::{Asset, DefaultValue, Field, Geometry, Guid, Record, Value, Widget, is_a, name_crc};

const CLASS_COMPONENT_INSTANCE: u32 = name_crc(b"ComponentInstance");
const CLASS_PROPERTY_VALUE: u32 = name_crc(b"PropertyValue");
const PROP_COMPONENTS: u32 = name_crc(b"Components");
const PROP_PROPERTY_VALUES: u32 = name_crc(b"PropertyValues");
const PROP_UNIQUE_ID: u32 = name_crc(b"UniqueId");
const PROP_COMPONENT_TEMPLATE: u32 = name_crc(b"ComponentTemplate");
const CLASS_EFFECT_INSTANCE: u32 = name_crc(b"EffectInstance");
const PROP_EFFECTS: u32 = name_crc(b"Effects");
const PROP_EFFECT_TEMPLATE: u32 = name_crc(b"EffectTemplate");
// PropertyValue: the override's path (u32 ids) and its variant value.
const PROP_PATH: u32 = name_crc(b"PropertyIdPath");
const PROP_VALUE: u32 = name_crc(b"Value");

/// One ComponentInstance (or EffectInstance) on a widget.
#[derive(Debug, Clone, PartialEq)]
pub struct Component {
    /// Object header span of the ComponentInstance.
    pub object: usize,
    /// Instance UniqueId; override paths start with it.
    pub uid: u32,
    /// The ComponentTemplate asset (`PHXCP`), e.g. StackLayout `d5481210…`.
    pub template: Option<Guid>,
    /// Instance name as authored ("Layout Component", "AlignmentUtility").
    pub name: String,
    /// The widget's overrides of this component's custom properties:
    /// (CustomPropertyInstance UniqueId, value). Properties not listed keep
    /// the template default.
    pub overrides: Vec<(u32, DefaultValue)>,
}

impl Component {
    /// The override for custom property `uid`, if the widget sets one.
    pub fn get(&self, uid: u32) -> Option<&DefaultValue> {
        self.overrides.iter().find(|o| o.0 == uid).map(|o| &o.1)
    }
}

/// A component custom property with its effective value.
#[derive(Debug, Clone, PartialEq)]
pub struct ComponentProp {
    pub uid: u32,
    pub name: String,
    pub value: DefaultValue,
    /// True when the widget overrides the template default.
    pub overridden: bool,
}

impl Asset {
    /// The widget's PropertyValues: (path of UniqueIds, value). A two-id path
    /// `[component uid, custom property uid]` overrides a component on this
    /// widget; on a ContainerWidgetInstance, paths starting with one of the
    /// template's own custom properties (object-typed data slots, listed in
    /// its `0xc3fa0951` array) feed data into the template instead.
    pub fn property_values(&self, widget: &Widget) -> Vec<(Vec<u32>, DefaultValue)> {
        let mut out = Vec::new();
        for (i, s) in self.spans.iter().enumerate() {
            let Record::Object(o) = &s.record else {
                continue;
            };
            if o.class != CLASS_PROPERTY_VALUE
                || !self.owned_by(i, widget.object, PROP_PROPERTY_VALUES)
            {
                continue;
            }
            let mut path = Vec::new();
            let mut value = None;
            for (k, t) in self.spans.iter().enumerate().skip(i + 1) {
                let Some(f) = t.field else { continue };
                if f.object != i {
                    if f.object < i {
                        break;
                    }
                    continue;
                }
                match (self.prop_name_crc(f), &t.record) {
                    (Some(PROP_PATH), Record::Value(Value::U32(v))) => path.push(*v),
                    (Some(PROP_VALUE), Record::Default(_)) if value.is_none() => {
                        value = Some(self.default_at(k))
                    }
                    _ => {}
                }
            }
            if let Some(v) = value {
                out.push((path, v));
            }
        }
        out
    }

    /// The ComponentInstances on a widget, with this widget's overrides.
    pub fn components(&self, widget: &Widget) -> Vec<Component> {
        self.instances(
            widget,
            CLASS_COMPONENT_INSTANCE,
            PROP_COMPONENTS,
            PROP_COMPONENT_TEMPLATE,
        )
    }

    /// The EffectInstances on a widget (`Effects`), shaped like components:
    /// `template` is the EffectTemplate (`PHXEF`), overrides address its
    /// custom properties the same way (`[effect uid, property uid]`).
    pub fn effects(&self, widget: &Widget) -> Vec<Component> {
        self.instances(
            widget,
            CLASS_EFFECT_INSTANCE,
            PROP_EFFECTS,
            PROP_EFFECT_TEMPLATE,
        )
    }

    fn instances(&self, widget: &Widget, class: u32, field: u32, template: u32) -> Vec<Component> {
        let values = self.property_values(widget);
        let mut out = Vec::new();
        for (i, s) in self.spans.iter().enumerate() {
            let Record::Object(o) = &s.record else {
                continue;
            };
            if o.class != class || !self.owned_by(i, widget.object, field) {
                continue;
            }
            let mut c = Component {
                object: i,
                uid: 0,
                template: None,
                name: String::new(),
                overrides: Vec::new(),
            };
            for p in self.object_props(i) {
                match (p.crc, p.value) {
                    (PROP_UNIQUE_ID, Value::U32(v)) => c.uid = v,
                    (t, Value::Ref(r)) if t == template => c.template = Some(r.id),
                    (_, Value::Str(s)) => c.name = s,
                    _ => {}
                }
            }
            c.overrides = values
                .iter()
                .filter(|(path, _)| path.len() == 2 && path[0] == c.uid)
                .map(|(path, v)| (path[1], v.clone()))
                .collect();
            out.push(c);
        }
        out
    }

    // Whether object span `i` is (an element of) property `prop` of `owner`.
    fn owned_by(&self, i: usize, owner: usize, prop: u32) -> bool {
        self.spans[i]
            .field
            .is_some_and(|f| f.object == owner && self.prop_name_crc(f) == Some(prop))
    }

    fn prop_name_crc(&self, f: Field) -> Option<u32> {
        let Record::Object(h) = &self.spans[f.object].record else {
            return None;
        };
        Some(
            self.class_props(h.class)?
                .get(f.prop as usize - 1)?
                .name_crc,
        )
    }
}

/// A component's custom properties: the ComponentTemplate asset's
/// defaults (`template` = the `PHXCP` asset the component references),
/// with the widget's overrides applied.
pub fn component_props(template: &Asset, component: &Component) -> Vec<ComponentProp> {
    template
        .properties()
        .iter()
        .map(|p| {
            let over = component.get(p.uid);
            ComponentProp {
                uid: p.uid,
                name: template.property_name(p).to_owned(),
                value: over
                    .cloned()
                    .unwrap_or_else(|| template.default_at(p.default)),
                overridden: over.is_some(),
            }
        })
        .collect()
}

// --------------------------------------------------------------------------
// Layout components

/// Items a list template shows with sample data: the game stamps its one
/// container child per element of its DataSource (0x30000001); 305 of the
/// 310 shipped list templates have exactly that shape.
pub const SAMPLE_LIST_ITEMS: usize = 3;
/// WidgetType of scrollable lists (FirstVisibleIndex, ScrollInfo); every
/// list type (List, Grid…) derives from it.
const LIST_WIDGET_TYPE: &str = "12d30959-97a2-470d-aee8-4374e2a67e84";
/// GameObjectLoader (`f521d20f…`): shows the child whose GameObjectTemplate
/// is its CurrentGameObject's.
const GAME_OBJECT_LOADER: &str = "f521d20f-3616-46fe-85a1-49655644eb43";
const PROP_GAME_OBJECT_TEMPLATE: u32 = name_crc(b"GameObjectTemplate");
/// Grid (`af18fdbb…`) and List (`dbcf9d78…`), the two list kinds; ids and
/// defaults from their shipped JSON sources (`data/ui/pwl/widgettypes`).
const LIST_TYPE: &str = "dbcf9d78-ce36-4fd1-aaa9-acba02520297";
const GRID_IS_HORIZONTAL: u32 = 910_266_698;
const GRID_HORIZONTAL_SPACING: u32 = 829_783_572;
const GRID_VERTICAL_SPACING: u32 = 868_418_658;
/// Maximum columns and rows the grid fills (default 2 each).
const GRID_COLUMNS: u32 = 857_636_566;
const GRID_ROWS: u32 = 904_124_647;
const LIST_IS_HORIZONTAL: u32 = 946_868_062;
const LIST_SPACING: u32 = 1_073_604_079;
/// Radial lists (`9eb9b942…`: StartAngle, TotalAngle, Radius): the
/// minimap's ring buttons, the construction radial.
const RADIAL_WIDGET_TYPE: &str = "9eb9b942-5726-4d76-8810-5207184b29ea";
/// WidgetType ScrollBar (`data/ui/pwl/widgettypes`). The game shows one
/// only when the list or panel it scrolls overflows; the pins list sizes
/// its slots by `IfTrueFalseAssign [8.5, 0, ScrollBar_DS · Visible]`, and
/// the reference HUD shows no bar under its few pins. The lists the editor
/// shows (empty, or a few sample items) fit, so bars start hidden.
/// TODO(alex): the native toggle is not traced (obfuscated code).
/// The InfoTipArea component on a hover area's template root (`AreaSize`,
/// `InfoTipAlignment`: game code sizes the area).
const INFOTIP_AREA: Guid = Guid::from_const("41f69505-a029-4bdb-bd17-6ab85420c93f");
const SCROLLBAR_WIDGET_TYPE: &str = "ad55d6e7-3783-4574-87fd-bcc313e2f606";
/// WidgetType InputText (`data/ui/pwl/widgettypes/inputtext_…`).
const INPUT_TEXT_WIDGET_TYPE: &str = "c9dfb3c3-84cb-4a99-a88b-ed1838f9f11f";
const RADIAL_START_ANGLE: u32 = 1_030_662_152;
const RADIAL_TOTAL_ANGLE: u32 = 989_470_054;
const RADIAL_RADIUS: u32 = 1_028_439_223;
/// A radial list's item groups (CAnnoSelectionWheelGroup `28ee1403…`,
/// components on the list instance): the minimap ring's day/night, speed,
/// controls, lodes and fertilities. See `wheel_slices`.
const WHEEL_GROUP: &str = "28ee1403-bdff-479d-be96-fad20734dc60";
const WHEEL_BEGIN_INDEX: u32 = 0x703e_da24;
const WHEEL_MIN_ITEMS: u32 = 0x7660_76b3;
const WHEEL_BEGIN_PADDING: u32 = 0x77c0_3b86;
const WHEEL_END_PADDING: u32 = 0x72a4_51d9;
const WHEEL_RADIUS_OFFSET: u32 = 0x7e62_b5ea;
const WHEEL_ALIGNMENT: u32 = 0x767d_30b4;
/// ItemAlignment entries: items fill a group's slots from its start (the
/// default), centred, or from its end.
const WHEEL_ALIGN_CENTER: u32 = 0x5675_1397;
const WHEEL_ALIGN_END: u32 = 0x53de_43ea;
/// The list's VisibleItems (`0x386336a4`, the scrollable list base): a
/// non-zero count moves every wheel slice back half a step (the wheel's
/// widget virtual +0x1F0 reads it, exe 2026-11-05 `+0x148`).
const LIST_VISIBLE_ITEMS: u32 = 0x3863_36a4;
/// Items a radial list shows with sample data: enough to read as a ring.
const SAMPLE_RADIAL_ITEMS: usize = 12;

/// Items a sample list stamps, `lists` sample lists deep. Nested lists
/// stamp fewer: the construction radial nests radial menus three deep, and
/// 12 items at every level built 590k nodes (45 s). One level down still
/// shows two, so spacing stays visible.
/// One CAnnoSelectionWheelGroup's settings, as the list instance holds
/// them after its parent's bindings (which chain each BeginIndex from the
/// group before).
#[derive(Debug, Clone, Copy, PartialEq)]
struct WheelGroup {
    begin: i32,
    min_items: i32,
    begin_padding: f32,
    end_padding: f32,
    radius_offset: f32,
    alignment: u32,
}

impl WheelGroup {
    fn read(uid: u32, values: &[(u32, u32, DefaultValue)]) -> WheelGroup {
        // Later entries (binding writes) override stored values.
        let get = |prop: u32| {
            values
                .iter()
                .rev()
                .find(|(c, p, _)| *c == uid && *p == prop)
                .map(|(_, _, v)| v)
        };
        let int = |prop| match get(prop) {
            Some(DefaultValue::S32(v)) => *v,
            Some(DefaultValue::U32(v)) => *v as i32,
            Some(DefaultValue::F32(b)) => f32::from_bits(*b) as i32,
            _ => 0,
        };
        let float = |prop| match get(prop) {
            Some(DefaultValue::F32(b)) => f32::from_bits(*b),
            Some(DefaultValue::S32(v)) => *v as f32,
            _ => 0.,
        };
        WheelGroup {
            begin: int(WHEEL_BEGIN_INDEX),
            min_items: int(WHEEL_MIN_ITEMS),
            begin_padding: float(WHEEL_BEGIN_PADDING),
            end_padding: float(WHEEL_END_PADDING),
            radius_offset: float(WHEEL_RADIUS_OFFSET),
            alignment: match get(WHEEL_ALIGNMENT) {
                Some(DefaultValue::U32(v)) => *v,
                _ => 0,
            },
        }
    }
}

/// Each item's slice of a radial list (centre angle, angle size, position),
/// None for an item no group covers. Degrees clockwise from up, y down.
///
/// The native selection wheel (`CAnnoSelectionWheelLayout`):
/// groups in BeginIndex order each span to the next one's BeginIndex (the
/// last to the end of the items) and take max(span, MinGroupItemCount)
/// slots. One step is TotalAngle less every group's Begin+EndPadding over
/// all slots. From StartAngle each group adds its BeginPadding, takes its
/// slots and adds its EndPadding; its items fill its slots from the start,
/// centred (rounded down) or from the end. An item sits at its slice's
/// centre on Radius + the group's RadiusOffset and turns to that angle.
/// Without groups the list is one group over all items.
/// With `windowed` (the list's VisibleItems set) every slice moves back half
/// a step.
fn wheel_slices(
    start: f32,
    total: f32,
    radius: f32,
    groups: &[WheelGroup],
    n: usize,
    windowed: bool,
) -> Vec<Option<(f32, f32, [f32; 2])>> {
    let (groups, spans, step) = wheel_spans(total, groups, n);
    if step <= 0. {
        return vec![None; n];
    }
    let mut spans = spans;
    let mut at = start - if windowed { step / 2. } else { 0. };
    for (g, s) in groups.iter().zip(&mut spans) {
        at += g.begin_padding;
        s.4 = at;
        at += s.2 as f32 * step + g.end_padding;
    }
    (0..n as i32)
        .map(|i| {
            let k = spans.iter().position(|s| s.0 <= i && i <= s.1)?;
            let (first, _, slots, items, from) = spans[k];
            let g = &groups[k];
            let mut d = i - first;
            match g.alignment {
                WHEEL_ALIGN_CENTER => d += (slots - items) / 2,
                WHEEL_ALIGN_END => d += slots - items,
                _ => {}
            }
            let a = from + (d as f32 + 0.5) * step;
            let r = a.to_radians();
            let rad = radius + g.radius_offset;
            Some((a, step, [rad * r.sin(), -rad * r.cos()]))
        })
        .collect()
}

/// One slice of the wheel (see [`wheel_slices`]).
fn wheel_step(total: f32, groups: &[WheelGroup], n: usize) -> f32 {
    wheel_spans(total, groups, n).2.max(0.)
}

/// The wheel's groups in order, each one's (first index, last index, slots,
/// items, start angle to fill in), and the step.
#[allow(clippy::type_complexity)]
fn wheel_spans(
    total: f32,
    groups: &[WheelGroup],
    n: usize,
) -> (Vec<WheelGroup>, Vec<(i32, i32, i32, i32, f32)>, f32) {
    let n_items = n as i32;
    let mut groups = groups.to_vec();
    if groups.is_empty() {
        groups.push(WheelGroup {
            begin: 0,
            min_items: 0,
            begin_padding: 0.,
            end_padding: 0.,
            radius_offset: 0.,
            alignment: 0,
        });
    }
    groups.sort_by_key(|g| g.begin);
    // (first index, last index, slots, items, start angle)
    let mut spans: Vec<(i32, i32, i32, i32, f32)> = Vec::with_capacity(groups.len());
    let mut remaining = n_items;
    let mut slots_total = 0;
    let mut free = total;
    for (k, g) in groups.iter().enumerate() {
        let span = match groups.get(k + 1) {
            Some(next) => next.begin - g.begin,
            None => remaining,
        };
        let slots = span.max(g.min_items);
        let items = span.min(remaining);
        spans.push((g.begin, g.begin + span - 1, slots, items, 0.));
        free -= g.begin_padding + g.end_padding;
        remaining -= items;
        slots_total += slots;
    }
    let step = if slots_total > 0 {
        free / slots_total as f32
    } else {
        0.
    };
    (groups, spans, step)
}

fn sample_count(radial: bool, lists: u8) -> usize {
    match (radial, lists) {
        (true, 0) => SAMPLE_RADIAL_ITEMS,
        (false, 0) => SAMPLE_LIST_ITEMS,
        (_, 1) => 2,
        _ => 1,
    }
}
/// The radial item component (`5065a655…`) the list writes per item.
pub(super) const RADIAL_ITEM: &str = "5065a655-d1cc-4a8a-824e-d167d02099ec";
const RADIAL_ITEM_POSITION: u32 = 2_008_061_936;
const RADIAL_ITEM_ROTATION: u32 = 1_888_713_560;
const RADIAL_ITEM_ANGLE_SIZE: u32 = 2_035_613_867;
/// The DynamicInteractiveWidgetIndexExposer component (`Index` on item
/// roots): the native list writes each item's DataSourceIndex into it, and
/// item templates pick their first / middle / last look from it against
/// TotalItemCount (the HUD archive column's grouped buttons).
const INDEX_EXPOSER: &str = "339a34b3-5055-4413-9067-cda264620401";
const INDEX_DATA_SOURCE: u32 = 0x71c4_877b;
/// A list's DataSource: the game-data array it stamps its items from.
const LIST_DATA_SOURCE: u32 = 0x3000_0001;
/// Interactive widget base (`6ca90344…`): items in the DataSource.
const LIST_TOTAL_ITEM_COUNT: u32 = 1_045_484_347;
/// The interface scale option (`W::[context, 0x7b1e8330]`, not in the Lua
/// stubs): icons are 16 × it, the governors bar picks its widths from
/// ranges 0.89–1.37 of it. Sample data runs at the default, 1.0.
const UI_SCALE: u32 = 0x7b1e_8330;

/// The item prototype of a list template: the root's only child, a
/// container instance. None when `asset` is not a list.
fn list_prototype(
    asset: &Asset,
    widgets: &[Widget],
    load: &mut dyn FnMut(Guid) -> Option<Arc<Asset>>,
) -> Option<usize> {
    let [only] = widgets.first()?.children[..] else {
        return None;
    };
    asset.container_template(&widgets[only])?;
    is_list(asset, load).then_some(only)
}

/// Whether the template's WidgetType derives from the scrollable list or
/// the radial list.
fn is_list(asset: &Asset, load: &mut dyn FnMut(Guid) -> Option<Arc<Asset>>) -> bool {
    derives_from(asset, load, LIST_WIDGET_TYPE) || derives_from(asset, load, RADIAL_WIDGET_TYPE)
}

/// Whether the template's WidgetType is `ty` or derives from it.
fn derives_from(
    asset: &Asset,
    load: &mut dyn FnMut(Guid) -> Option<Arc<Asset>>,
    target: &'static str,
) -> bool {
    // Asked per instance for every template; a template's WidgetType chain
    // never changes, so the answer is kept per (template, target).
    thread_local! {
        static KNOWN: std::cell::RefCell<HashMap<(Guid, &'static str), bool>> =
            std::cell::RefCell::new(HashMap::new());
    }
    let key = asset.guid().map(|g| (g, target));
    if let Some(k) = key
        && let Some(hit) = KNOWN.with(|m| m.borrow().get(&k).copied())
    {
        return hit;
    }
    let found = (|| {
        let want = Guid::parse(target)?;
        let mut ty = asset.widget_type()?.id;
        // WidgetType chains are a handful deep; the bound guards a cycle.
        for _ in 0..8 {
            if ty == want {
                return Some(true);
            }
            ty = load(ty)?.widget_type()?.id;
        }
        Some(false)
    })()
    .unwrap_or(false);
    if let Some(k) = key {
        KNOWN.with(|m| m.borrow_mut().insert(k, found));
    }
    found
}

/// CustomPropertyType of booleans.
const PROPERTY_BOOL: u32 = 0;

/// ComponentTemplate guids of the layout components the evaluator applies.
pub mod templates {
    pub const STACK_LAYOUT: &str = "d5481210-2f09-4e19-91f0-f98e3633a6b7";
    pub const ALIGNMENT_UTILITY: &str = "b973e271-9d9d-454b-82a2-f28ba5347b29";
    pub const PADDING_UTILITY: &str = "60a1f9fe-76bc-47e5-8810-791d2b3da57b";
    pub const PROPORTIONAL_PIVOT: &str = "c7b1e237-d1a6-4d84-b103-151d80f94439";
    pub const BACKGROUND_LAYOUT: &str = "33ee79a6-ebdc-4aae-8fd7-0c41b6dc1dd8";
}

// Custom property UniqueIds (ComponentTemplate JSON, also immediates in the
// native property descriptors).
const STACK_ORIENTATION: u32 = 2_086_100_441;
const STACK_CHILDREN_ALIGNMENT: u32 = 2_116_384_459;
const STACK_INVERT_ORDER: u32 = 1_907_223_739;
const STACK_SPACE_BETWEEN: u32 = 2_115_500_691;
const STACK_GROUP_MARGIN: u32 = 1_965_506_629;
const ALIGNMENT: u32 = 2_020_197_799;
const PADDING: u32 = 652_292_905;
const PROPORTIONAL_PIVOT: u32 = 1_965_950_531;
const BACKGROUND_MARGIN: u32 = 2_098_838_388;

// Enum entry values (EnumAssets Orientation c70b4932…, LayoutAlignment
// 46e304bb…).
const HORIZONTAL: u32 = 0x59ba_6b33;
const ALIGN_INHERITED: u32 = 0x5660_7d2a;
const ALIGN_BEGIN: u32 = 0x53ad_f3bd;
const ALIGN_MIDDLE: u32 = 0x5995_f659;
const ALIGN_END: u32 = 0x5224_ca23;

/// Cross-axis alignment of a stack item.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Align {
    Inherited,
    Begin,
    Middle,
    End,
}

impl Align {
    fn from_entry(v: u32) -> Align {
        match v {
            ALIGN_MIDDLE => Align::Middle,
            ALIGN_END => Align::End,
            ALIGN_INHERITED => Align::Inherited,
            _ => Align::Begin,
        }
    }
}

/// The layout-relevant components, decoded with the template defaults.
#[derive(Debug, Clone, PartialEq)]
pub enum LayoutComponent {
    /// On a parent: places its children one after another.
    Stack {
        horizontal: bool,
        align: Align,
        invert: bool,
        spacing: f32,
        /// left, top, right, bottom.
        margin: [f32; 4],
    },
    /// On a stack item: overrides the stack's cross-axis alignment.
    Alignment(Align),
    /// On a stack item: empty space around it (left, top, right, bottom).
    Padding([f32; 4]),
    /// Pivot as a ratio of the widget's size.
    ProportionalPivot([f32; 3]),
    /// On a parent: its first child becomes the background of the others,
    /// grown by `margin` (left, top, right, bottom).
    Background { margin: [f32; 4] },
    /// On a Grid list: items in cells of the largest item's size. With
    /// `across` they fill a row of `columns` cells, then the next row;
    /// without, a column of `rows` cells, then the next column.
    Grid {
        across: bool,
        columns: usize,
        rows: usize,
        spacing: [f32; 2],
    },
}

fn floats(v: Option<&DefaultValue>) -> Option<Vec<f32>> {
    match v? {
        DefaultValue::Vec(x) => Some(x.iter().map(|b| f32::from_bits(*b)).collect()),
        DefaultValue::F32(b) => Some(vec![f32::from_bits(*b)]),
        _ => None,
    }
}

fn u32_of(v: Option<&DefaultValue>) -> Option<u32> {
    match v? {
        DefaultValue::U32(x) => Some(*x),
        DefaultValue::S32(x) => Some(*x as u32),
        _ => None,
    }
}

fn spacing4(v: Option<&DefaultValue>) -> [f32; 4] {
    let f = floats(v).unwrap_or_default();
    std::array::from_fn(|i| f.get(i).copied().unwrap_or(0.0))
}

impl LayoutComponent {
    /// Decodes a layout component; None for every other kind. Defaults are
    /// the ComponentTemplate defaults (all shipped layout templates).
    pub fn from_component(c: &Component) -> Option<LayoutComponent> {
        const STACK: Guid = Guid::from_const(templates::STACK_LAYOUT);
        const ALIGNMENT_UTILITY: Guid = Guid::from_const(templates::ALIGNMENT_UTILITY);
        const PADDING_UTILITY: Guid = Guid::from_const(templates::PADDING_UTILITY);
        const PIVOT: Guid = Guid::from_const(templates::PROPORTIONAL_PIVOT);
        const BACKGROUND: Guid = Guid::from_const(templates::BACKGROUND_LAYOUT);
        Some(match c.template? {
            STACK => LayoutComponent::Stack {
                horizontal: u32_of(c.get(STACK_ORIENTATION)).unwrap_or(HORIZONTAL) == HORIZONTAL,
                align: Align::from_entry(
                    u32_of(c.get(STACK_CHILDREN_ALIGNMENT)).unwrap_or(ALIGN_BEGIN),
                ),
                invert: matches!(c.get(STACK_INVERT_ORDER), Some(DefaultValue::Bool(b)) if *b != 0),
                spacing: u32_of(c.get(STACK_SPACE_BETWEEN)).unwrap_or(0) as f32,
                margin: spacing4(c.get(STACK_GROUP_MARGIN)),
            },
            ALIGNMENT_UTILITY => LayoutComponent::Alignment(Align::from_entry(
                u32_of(c.get(ALIGNMENT)).unwrap_or(ALIGN_INHERITED),
            )),
            PADDING_UTILITY => LayoutComponent::Padding(spacing4(c.get(PADDING))),
            PIVOT => {
                let f = floats(c.get(PROPORTIONAL_PIVOT)).unwrap_or_default();
                LayoutComponent::ProportionalPivot(std::array::from_fn(|i| {
                    f.get(i).copied().unwrap_or(0.0)
                }))
            }
            BACKGROUND => LayoutComponent::Background {
                margin: spacing4(c.get(BACKGROUND_MARGIN)),
            },
            _ => return None,
        })
    }
}

/// One node of a (possibly cross-file) widget tree to lay out.
#[derive(Debug, Clone, Default)]
pub struct LayoutNode {
    pub geometry: Geometry,
    /// The widget's own box before Scale, `[0, intrinsic]`: ImageAsset
    /// `Size`, a label's `DisplayedSize`, a container's template
    /// `ReferenceSize` when the template's bool `0x072d960f` is set. None =
    /// the bounds of its children.
    pub intrinsic: Option<[f32; 2]>,
    /// Where the intrinsic box starts: 0, except for a container a bound
    /// Size scales over its content, whose box stays its children's union
    /// (the lode badge's art spans −77..77 around its origin).
    pub origin: [f32; 2],
    /// The Size its children anchor in (runtime `CompositeWidget` +0x1e0):
    /// a container's template `ReferenceSize` whatever that bool says, the
    /// screen for the root. Plain groups have none (anchors add nothing);
    /// a StackLayout sets it to the stacked size.
    pub size: Option<[f32; 2]>,
    pub components: Vec<LayoutComponent>,
    /// Indices into the node list, in WidgetInstances (array) order.
    pub children: Vec<usize>,
    /// Values the template's data bindings put over the stored geometry
    /// (already applied to `geometry`, except `size`, which `evaluate`
    /// turns into Scale once the intrinsic size is known).
    pub bound: super::binding::Bound,
    /// A label text bindings put there (game data or preview values), or
    /// for an image widget the image path they set.
    pub text: Option<String>,
    /// A label LocalizationId bindings put there: the line it names wins
    /// over `text` where the text table has it.
    pub loc: Option<String>,
    /// 0 for a widget of its own; k for the k-th sample list item stamped
    /// from the list's prototype widget (shares its `source`; not a widget
    /// of its own). The items differ only by this.
    pub item: u32,
}

/// An axis-aligned rectangle in the root's pixel space (y down).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Rect {
    pub min: [f32; 2],
    pub max: [f32; 2],
}

impl Rect {
    pub fn size(&self) -> [f32; 2] {
        [self.max[0] - self.min[0], self.max[1] - self.min[1]]
    }
    fn union(self, o: Rect) -> Rect {
        Rect {
            min: [self.min[0].min(o.min[0]), self.min[1].min(o.min[1])],
            max: [self.max[0].max(o.max[0]), self.max[1].max(o.max[1])],
        }
    }
}

/// Result per node: the 2D affine transform from the node's own pixel
/// space to the root's (`[a, b, c, d, tx, ty]`: x' = a·x + c·y + tx,
/// y' = b·x + d·y + ty), its local box (`[0, intrinsic]`, or the union of
/// its visible children), and that box's root-space bounds.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Placed {
    pub transform: [f32; 6],
    pub local: Rect,
    pub rect: Rect,
}

/// Lays out the tree under `root` (sized `root_size`: the game sizes every
/// scene root 1920×1080 and scales the result to the
/// viewport). Unreached nodes keep `Placed::default()`.
///
/// Per node, in its parent's pixels:
///
/// ```text
/// p' = anchor(AnchorPoint, parent Size) + Position / m
///      + c ⊙ R(rot) · (Scale ⊙ (p − Pivot))
/// ```
///
/// `anchor` picks a 3×3 grid point of the parent's Size.
/// `m` and `c` cancel ancestor scale when the anchor/resize metrics ask for
/// it; both are 1 in an unscaled tree.
/// Pivot comes from ProportionalPivotPoint when present (box min + ratio ×
/// box size). Then the parent's StackLayout / BackgroundLayout move the
/// children.
///
/// Bounds use the same transform. The game's bounds path
/// skips `m` and `c` for anchored or counter-scaled
/// children; that only differs under scaled ancestors.
pub fn evaluate(nodes: &[LayoutNode], root: usize, root_size: [f32; 2]) -> Vec<Placed> {
    evaluate_where(nodes, root, root_size, &|_| true)
}

/// [`evaluate`], not going below the nodes `descend` refuses: they are
/// placed with no children (a box of their own size, if any), and what is
/// below them keeps `Placed::default()`. For a hidden node that only
/// changes what is hidden: hidden children take no part in their parent's
/// box or stacking.
pub fn evaluate_where(
    nodes: &[LayoutNode],
    root: usize,
    root_size: [f32; 2],
    descend: &dyn Fn(usize) -> bool,
) -> Vec<Placed> {
    evaluate_since(nodes, root, root_size, descend, &[], None, &|_| false).placed
}

/// What [`evaluate_since`] worked out, per node, for the next layout to
/// take over: placements, and on the way the fits, boxes and local
/// transforms, with the shape they were worked out on.
#[derive(Default)]
pub struct Laid {
    pub placed: Vec<Placed>,
    fits: Vec<Fit>,
    /// What each fit was worked out from: (cumulative scale, parent scale).
    fit_in: Vec<([f32; 2], [f32; 2])>,
    boxes: Vec<Option<Rect>>,
    local: Vec<[f32; 6]>,
    /// The nodes whose fit was worked out (or came over), and those it
    /// went below.
    valid: Vec<u64>,
    down: Vec<u64>,
    /// Children by node: `child_list[child_start[i]..child_start[i + 1]]`.
    child_start: Vec<u32>,
    child_list: Vec<u32>,
}

fn on(set: &[u64], i: usize) -> bool {
    set.get(i / 64).is_some_and(|w| w >> (i % 64) & 1 == 1)
}

fn put(set: &mut [u64], i: usize) {
    set[i / 64] |= 1 << (i % 64);
}

fn bits(of: impl IntoIterator<Item = bool>) -> Vec<u64> {
    let mut set = Vec::new();
    for (i, b) in of.into_iter().enumerate() {
        if i % 64 == 0 {
            set.push(0);
        }
        set[i / 64] |= u64::from(b) << (i % 64);
    }
    set
}

/// [`evaluate_where`], taking over from `last`, the result for the tree
/// `prev_of` maps nodes to ([`Tree::previous`]): every node starts from
/// what it was there, and the passes stop at a subtree whose nodes and
/// children are all the same (and not `changed`) where what comes from
/// above is the same too. A relayout after a hover or an edit works out the
/// few nodes it moved.
pub fn evaluate_since(
    nodes: &[LayoutNode],
    root: usize,
    root_size: [f32; 2],
    descend: &dyn Fn(usize) -> bool,
    prev_of: &[u32],
    last: Option<&Laid>,
    changed: &dyn Fn(usize) -> bool,
) -> Laid {
    let n = nodes.len();
    let words = n.div_ceil(64);
    let mut down = vec![0u64; words];
    let mut child_start = Vec::with_capacity(n + 1);
    let mut child_list = Vec::with_capacity(n);
    for (i, node) in nodes.iter().enumerate() {
        if descend(i) {
            put(&mut down, i);
        }
        child_start.push(child_list.len() as u32);
        child_list.extend(node.children.iter().map(|&c| c as u32));
    }
    child_start.push(child_list.len() as u32);
    let was = |i: usize| match prev_of.get(i) {
        Some(&m) if m != NOT_KEPT => Some(m as usize),
        _ => None,
    };
    let mut fits = vec![Fit::default(); n];
    let mut fit_in = vec![([0.; 2], [0.; 2]); n];
    let mut boxes: Vec<Option<Rect>> = vec![None; n];
    let mut local: Vec<[f32; 6]> = vec![IDENTITY; n];
    let mut out = vec![Placed::default(); n];
    let mut valid = vec![0u64; words];
    let mut clean = vec![0u64; words];
    if let Some(last) = last {
        // Start from what each node was.
        for i in 0..n {
            let Some(m) = was(i).filter(|&m| m < last.fits.len()) else {
                continue;
            };
            fits[i] = last.fits[m];
            fit_in[i] = last.fit_in[m];
            boxes[i] = last.boxes[m];
            local[i] = last.local[m];
            out[i] = last.placed[m];
            if on(&last.valid, m) {
                put(&mut valid, i);
            }
        }
        // Same subtrees, bottom up: children come after their parents.
        for i in (0..n).rev() {
            let Some(m) = was(i).filter(|&m| on(&valid, i) && m + 1 < last.child_start.len())
            else {
                continue;
            };
            let d = on(&down, i);
            let kids = &nodes[i].children;
            let theirs =
                &last.child_list[last.child_start[m] as usize..last.child_start[m + 1] as usize];
            let same = !changed(i)
                && d == on(&last.down, m)
                && (!d
                    || (kids.len() == theirs.len()
                        && kids
                            .iter()
                            .zip(theirs)
                            .all(|(&c, &t)| prev_of.get(c) == Some(&t) && on(&clean, c))));
            if same {
                put(&mut clean, i);
            }
        }
    }
    let mut inc = Inc {
        clean,
        valid,
        fit_kept: vec![0u64; words],
        box_kept: vec![0u64; words],
        fit_in,
        descend,
    };
    fit(nodes, root, [1.0; 2], [1.0; 2], &mut fits, 0, &mut inc);
    let root_box = Rect {
        min: [0.0; 2],
        max: root_size,
    };
    Pass {
        nodes,
        fits: &fits,
        boxes: &mut boxes,
        local: &mut local,
        descend,
        inc: &mut inc,
    }
    .box_of(root, Some(root_box), Some(root_size), 0);
    place(nodes, root, IDENTITY, &boxes, &local, &mut out, 0, &mut inc);
    Laid {
        placed: out,
        fits,
        fit_in: inc.fit_in,
        boxes,
        local,
        valid: inc.valid,
        down,
        child_start,
        child_list,
    }
}

/// Where [`evaluate_since`] stops: same subtrees, and the roots of those
/// whose fits and boxes held (everything below them already does).
struct Inc<'a> {
    clean: Vec<u64>,
    valid: Vec<u64>,
    fit_kept: Vec<u64>,
    box_kept: Vec<u64>,
    fit_in: Vec<([f32; 2], [f32; 2])>,
    descend: &'a dyn Fn(usize) -> bool,
}

const IDENTITY: [f32; 6] = [1.0, 0.0, 0.0, 1.0, 0.0, 0.0];
// Cross-file trees are deep but finite; this only stops cycles.
const MAX_DEPTH: usize = 64;

fn mul(p: [f32; 6], c: [f32; 6]) -> [f32; 6] {
    [
        p[0] * c[0] + p[2] * c[1],
        p[1] * c[0] + p[3] * c[1],
        p[0] * c[2] + p[2] * c[3],
        p[1] * c[2] + p[3] * c[3],
        p[0] * c[4] + p[2] * c[5] + p[4],
        p[1] * c[4] + p[3] * c[5] + p[5],
    ]
}

fn apply(m: [f32; 6], p: [f32; 2]) -> [f32; 2] {
    [
        m[0] * p[0] + m[2] * p[1] + m[4],
        m[1] * p[0] + m[3] * p[1] + m[5],
    ]
}

fn bounds(m: [f32; 6], b: Rect) -> Rect {
    let corners = [b.min, [b.max[0], b.min[1]], [b.min[0], b.max[1]], b.max].map(|c| apply(m, c));
    corners[1..].iter().fold(
        Rect {
            min: corners[0],
            max: corners[0],
        },
        |r, &c| r.union(Rect { min: c, max: c }),
    )
}

/// The offset AnchorPoint `a` adds to Position: 0 top-left, 1 top-centre,
/// 2 top-right, 3 middle-left … 8 bottom-right of `size` (the parent's
/// Size; out-of-range values add nothing).
pub fn anchor_offset(a: u32, size: [f32; 2]) -> [f32; 2] {
    if a > 8 {
        return [0.0; 2];
    }
    [
        (a % 3) as f32 * 0.5 * size[0],
        (a / 3) as f32 * 0.5 * size[1],
    ]
}

// Position and extra scale after the metrics fields cancel ancestor scale.
#[derive(Debug, Clone, Copy)]
struct Fit {
    position: [f32; 2],
    counter: [f32; 2],
}

impl Default for Fit {
    fn default() -> Fit {
        Fit {
            position: [0.0; 2],
            counter: [1.0; 2],
        }
    }
}

// Top-down: `cum` is Π (Scale ⊙ counter) over the parent and its ancestors,
// `parent` the parent's own Scale.
//
// Anchor metrics: 1 → Position / cum, 2 → Position /
// parent Scale. AspectRatio: pick a reference axis (1:
// the smaller cum, 2: x, 3: y) and scale the other axis by cum[ref] /
// cum[other], so the widget ends up uniformly scaled. Otherwise resize
// metrics per axis: 1 → 1 / cum, 2 → 1 / parent Scale. Division by zero
// yields 0, as in the game.
fn fit(
    nodes: &[LayoutNode],
    i: usize,
    cum: [f32; 2],
    parent: [f32; 2],
    out: &mut [Fit],
    depth: usize,
    inc: &mut Inc,
) {
    // The same subtree from the same scales: all of it holds.
    if on(&inc.clean, i) && inc.fit_in[i] == (cum, parent) {
        put(&mut inc.fit_kept, i);
        return;
    }
    inc.fit_in[i] = (cum, parent);
    put(&mut inc.valid, i);
    let descend = inc.descend;
    let g = &nodes[i].geometry;
    let div = |v: f32, d: f32| if d == 0.0 { 0.0 } else { v / d };
    let by = |metric: u32, axis: usize| match metric {
        1 => Some(cum[axis]),
        2 => Some(parent[axis]),
        _ => None,
    };
    let mut f = Fit::default();
    for a in 0..2 {
        f.position[a] = match by(g.anchor_metrics[a], a) {
            Some(d) => div(g.position[a], d),
            None => g.position[a],
        };
    }
    if depth > 0 {
        if g.aspect_ratio != 0 {
            let r = match g.aspect_ratio {
                1 => usize::from(cum[0] > cum[1]),
                3 => 1,
                _ => 0,
            };
            f.counter[1 - r] = div(cum[r], cum[1 - r]);
        } else {
            for a in 0..2 {
                if let Some(d) = by(g.resize_metrics[a], a) {
                    f.counter[a] = div(1.0, d);
                }
            }
        }
    }
    out[i] = f;
    if depth >= MAX_DEPTH || !descend(i) {
        return;
    }
    let scale = scale(&nodes[i]);
    let next = [
        cum[0] * scale[0] * f.counter[0],
        cum[1] * scale[1] * f.counter[1],
    ];
    for &c in &nodes[i].children {
        if c < nodes.len() {
            fit(nodes, c, next, scale, out, depth + 1, inc);
        }
    }
}

/// The node's Scale: a bound Size is the drawn size, so Scale = size /
/// intrinsic on each axis that has one (as the game's Size setter does).
fn scale(n: &LayoutNode) -> [f32; 2] {
    let mut s = [n.geometry.scale[0], n.geometry.scale[1]];
    if let (Some(size), Some(i)) = (n.bound.size, n.intrinsic) {
        for a in 0..2 {
            if i[a] > 0.0 {
                s[a] = size[a] / i[a];
            }
        }
    }
    s
}

// Node-local transform into its parent's pixel space; `own` is the node's
// local box, `size` the parent's Size.
fn local_transform(n: &LayoutNode, f: Fit, own: Rect, size: [f32; 2]) -> [f32; 6] {
    let g = &n.geometry;
    let mut pivot = [g.pivot[0], g.pivot[1]];
    for c in &n.components {
        if let LayoutComponent::ProportionalPivot(r) = c {
            let s = own.size();
            pivot = [own.min[0] + r[0] * s[0], own.min[1] + r[1] * s[1]];
        }
    }
    let at = anchor_offset(g.anchor, size);
    // Most widgets are unrotated; sin_cos(0) is exactly (0, 1).
    let (s, c) = if g.rotation == 0.0 {
        (0.0, 1.0)
    } else {
        g.rotation.to_radians().sin_cos()
    };
    let [sx, sy] = scale(n);
    let [kx, ky] = f.counter;
    // T(at + pos) · diag(counter) · R · S · T(-pivot): the counter scale
    // applies after rotation, about the pivot.
    let m = [
        kx * c * sx,
        ky * s * sx,
        -kx * s * sy,
        ky * c * sy,
        0.0,
        0.0,
    ];
    let t = apply(m, [-pivot[0], -pivot[1]]);
    [
        m[0],
        m[1],
        m[2],
        m[3],
        t[0] + at[0] + f.position[0],
        t[1] + at[1] + f.position[1],
    ]
}

struct Pass<'a, 'b> {
    nodes: &'b [LayoutNode],
    fits: &'b [Fit],
    boxes: &'b mut Vec<Option<Rect>>,
    local: &'b mut Vec<[f32; 6]>,
    descend: &'b dyn Fn(usize) -> bool,
    inc: &'b mut Inc<'a>,
}

impl Pass<'_, '_> {
    // Computes the local box of `i` and the local transforms of its
    // children, running its layout components. `forced` / `size` override
    // the node's own box and Size (the root).
    fn box_of(
        &mut self,
        i: usize,
        forced: Option<Rect>,
        size: Option<[f32; 2]>,
        depth: usize,
    ) -> Rect {
        let nodes = self.nodes;
        if on(&self.inc.fit_kept, i) {
            if forced.is_none() {
                // Its box and what is below it hold; its own local
                // transform is its parent's to work out.
                put(&mut self.inc.box_kept, i);
                return self.boxes[i].unwrap_or_default();
            }
            // The root, whose box the screen forces: its children's fits
            // held with it.
            for &c in &nodes[i].children {
                if c < nodes.len() {
                    put(&mut self.inc.fit_kept, c);
                }
            }
        }
        let n = &nodes[i];
        let own = forced.or(n.intrinsic.map(|s| Rect {
            min: n.origin,
            max: [n.origin[0] + s[0], n.origin[1] + s[1]],
        }));
        let kids: &[usize] = if (self.descend)(i) { &n.children } else { &[] };
        if depth < MAX_DEPTH {
            for &c in kids {
                self.box_of(c, None, None, depth + 1);
            }
        }
        // Children anchor in the parent's Size, not its bounds: a container
        // whose box is its children's union still anchors in its template's
        // ReferenceSize.
        let size = size.or(n.size).unwrap_or_default();
        for &c in kids {
            self.local[c] = local_transform(
                &nodes[c],
                self.fits[c],
                self.boxes[c].unwrap_or_default(),
                size,
            );
        }
        let mut stacked = None;
        for comp in &n.components {
            match comp {
                LayoutComponent::Stack { .. } => {
                    stacked = Some(stack(nodes, comp, kids, self.boxes, self.local));
                }
                LayoutComponent::Grid { .. } => {
                    stacked = Some(grid(nodes, comp, kids, self.boxes, self.local));
                }
                LayoutComponent::Background { margin } => {
                    background(nodes, *margin, kids, self.boxes, self.local);
                }
                _ => {}
            }
        }
        let b = own.or(stacked).unwrap_or_else(|| {
            kids.iter()
                .filter(|&&c| counts(&nodes[c]))
                .map(|&c| bounds(self.local[c], self.boxes[c].unwrap_or_default()))
                .reduce(Rect::union)
                .unwrap_or_default()
        });
        self.boxes[i] = Some(b);
        b
    }
}

// StackLayout: items in reverse
// array order (InvertOrder: array order); each item's bounds start at the
// cursor (plus its padding), the cursor starts at 0 and advances by the
// item's extent + padding + spacing; hidden items take no space. Cross axis:
// Begin / Middle / End of the largest item. The group's
// box is then fixed to origin (−left,
// −top), size content + margins: GroupMargin grows the box outward and
// does not move the items.
fn stack(
    nodes: &[LayoutNode],
    comp: &LayoutComponent,
    kids: &[usize],
    boxes: &[Option<Rect>],
    local: &mut [[f32; 6]],
) -> Rect {
    let LayoutComponent::Stack {
        horizontal,
        align,
        invert,
        spacing,
        margin,
    } = *comp
    else {
        return Rect::default();
    };
    let (main, cross) = if horizontal { (0, 1) } else { (1, 0) };
    let order: Vec<usize> = if invert {
        kids.to_vec()
    } else {
        kids.iter().rev().copied().collect()
    };
    let mut cursor = 0.0;
    let mut cross_max: f32 = 0.0;
    let mut placed = Vec::new();
    for &c in &order {
        if !counts(&nodes[c]) {
            continue;
        }
        let pad = nodes[c]
            .components
            .iter()
            .find_map(|x| match x {
                LayoutComponent::Padding(p) => Some(*p),
                _ => None,
            })
            .unwrap_or([0.0; 4]);
        let r = bounds(local[c], boxes[c].unwrap_or_default());
        let ext = r.size();
        let mut target = [0.0; 2];
        target[main] = cursor + pad[main];
        target[cross] = pad[cross];
        local[c][4] += target[0] - r.min[0];
        local[c][5] += target[1] - r.min[1];
        let pad_main = pad[main] + pad[main + 2];
        let pad_cross = pad[cross] + pad[cross + 2];
        cursor += ext[main] + pad_main + spacing;
        cross_max = cross_max.max(ext[cross] + pad_cross);
        placed.push((c, ext[cross] + pad_cross));
    }
    if !placed.is_empty() {
        cursor -= spacing;
    }
    for (c, ext) in placed {
        let own = nodes[c].components.iter().find_map(|x| match x {
            LayoutComponent::Alignment(a) if *a != Align::Inherited => Some(*a),
            _ => None,
        });
        let off = match own.unwrap_or(align) {
            Align::Middle => (cross_max - ext) * 0.5,
            Align::End => cross_max - ext,
            Align::Begin | Align::Inherited => 0.0,
        };
        local[c][4 + cross] += off;
    }
    let mut content = [0.0; 2];
    content[main] = cursor;
    content[cross] = cross_max;
    Rect {
        min: [-margin[0], -margin[1]],
        max: [content[0] + margin[2], content[1] + margin[3]],
    }
}

// Grid list: what the native Grid widget type does with its items, from
// its documented properties (IsHorizontal: "display the items in the
// columns, then the next row"; Horizontal/VerticalVisibleItemCount: the
// most columns/rows it instantiates).
// TODO(alex): cell size as the largest item and array order are assumed;
// the native grid code is not read.
fn grid(
    nodes: &[LayoutNode],
    comp: &LayoutComponent,
    kids: &[usize],
    boxes: &[Option<Rect>],
    local: &mut [[f32; 6]],
) -> Rect {
    let LayoutComponent::Grid {
        across,
        columns,
        rows,
        spacing,
    } = *comp
    else {
        return Rect::default();
    };
    let items: Vec<(usize, Rect)> = kids
        .iter()
        .filter(|&&c| counts(&nodes[c]))
        .map(|&c| (c, bounds(local[c], boxes[c].unwrap_or_default())))
        .collect();
    let cell = items.iter().fold([0f32; 2], |m, (_, r)| {
        let s = r.size();
        [m[0].max(s[0]), m[1].max(s[1])]
    });
    let (mut used_c, mut used_r) = (0, 0);
    for (k, (c, r)) in items.iter().enumerate() {
        let (col, row) = if across {
            (k % columns.max(1), k / columns.max(1))
        } else {
            (k / rows.max(1), k % rows.max(1))
        };
        used_c = used_c.max(col + 1);
        used_r = used_r.max(row + 1);
        let target = [
            col as f32 * (cell[0] + spacing[0]),
            row as f32 * (cell[1] + spacing[1]),
        ];
        local[*c][4] += target[0] - r.min[0];
        local[*c][5] += target[1] - r.min[1];
    }
    let extent = |n: usize, a: usize| {
        if n == 0 {
            0.
        } else {
            n as f32 * cell[a] + (n - 1) as f32 * spacing[a]
        }
    };
    Rect {
        min: [0.; 2],
        max: [extent(used_c, 0), extent(used_r, 1)],
    }
}

// BackgroundLayout: the first child (array order; the last layer in the
// editor's layer panel) is stretched over the bounds of the other children
// grown by `margin`.
fn background(
    nodes: &[LayoutNode],
    margin: [f32; 4],
    kids: &[usize],
    boxes: &[Option<Rect>],
    local: &mut [[f32; 6]],
) {
    let Some((&bg, rest)) = kids.split_first() else {
        return;
    };
    let Some(content) = rest
        .iter()
        .filter(|&&c| counts(&nodes[c]))
        .map(|&c| bounds(local[c], boxes[c].unwrap_or_default()))
        .reduce(Rect::union)
    else {
        return;
    };
    let target = Rect {
        min: [content.min[0] - margin[0], content.min[1] - margin[1]],
        max: [content.max[0] + margin[2], content.max[1] + margin[3]],
    };
    let own = boxes[bg].unwrap_or_default();
    let (os, ts) = (own.size(), target.size());
    let sx = if os[0] > 0.0 { ts[0] / os[0] } else { 1.0 };
    let sy = if os[1] > 0.0 { ts[1] / os[1] } else { 1.0 };
    local[bg] = [
        sx,
        0.0,
        0.0,
        sy,
        target.min[0] - sx * own.min[0],
        target.min[1] - sy * own.min[1],
    ];
}

// Whether a child takes part in its parent's bounds and stacking. The game
// tests only ConsiderSize (node flag 0x200);
// Visible does not touch layout. Statically hidden layers
// are mostly animation or binding placeholders whose runtime state differs
// (maintitle's hidden `Gradient` is 20,352 px tall), so they are skipped
// here as the closest static approximation.
fn counts(n: &LayoutNode) -> bool {
    n.geometry.visible && !n.geometry.hidden && n.geometry.consider_size
}

fn place(
    nodes: &[LayoutNode],
    i: usize,
    parent: [f32; 6],
    boxes: &[Option<Rect>],
    local: &[[f32; 6]],
    out: &mut [Placed],
    depth: usize,
    inc: &mut Inc,
) {
    // Its subtree's boxes held, and it sits where it did: so does all of
    // it.
    if on(&inc.box_kept, i) && out[i].transform == parent {
        return;
    }
    let descend = inc.descend;
    let b = boxes[i].unwrap_or_default();
    out[i] = Placed {
        transform: parent,
        local: b,
        rect: bounds(parent, b),
    };
    if depth >= MAX_DEPTH || !descend(i) {
        return;
    }
    for &c in &nodes[i].children {
        if c < nodes.len() {
            place(
                nodes,
                c,
                mul(parent, local[c]),
                boxes,
                local,
                out,
                depth + 1,
                inc,
            );
        }
    }
}

/// A layout tree spanning files: one asset's widgets with every container
/// expanded from its template, as the game instantiates them
/// (the template's top-level widgets become the
/// instance's children, its ReferenceSize the instance's Size).
#[derive(Default)]
pub struct Tree {
    pub nodes: Vec<LayoutNode>,
    /// Per node: (file, index into that file's `widgets()`). File 0 is the
    /// asset the tree was built from, file `k` is `templates[k - 1]`.
    pub source: Vec<(usize, usize)>,
    /// Per node: the container node whose template it comes from (None for
    /// the asset's own widgets).
    pub instance: Vec<Option<usize>>,
    /// Per node: the node it lays out in. Template roots have none; their
    /// children hang under the container instance.
    pub parent: Vec<Option<usize>>,
    pub templates: Vec<Arc<Asset>>,
    /// Data bindings were evaluated.
    pub bindings: bool,
    pub play: Option<Play>,
    pub preview: Preview,
    pub shown: bool,
    pub sims: Vec<Sim>,
    pub sample: Option<Sampler>,
    /// Per node, from a first layout pass: the drawn size bindings read for
    /// widgets whose Size nothing binds (see [`Tree::build_opts`]).
    sizes: Vec<[f32; 2]>,
    /// Per node, from the same pass: where a container's union box starts.
    mins: Vec<[f32; 2]>,
    /// Per node, from the same pass: its transform to the root, for nodes
    /// a Positioning component reads (NaN elsewhere).
    transforms: Vec<[f32; 6]>,
    /// Per instance key: its template's property values as evaluated, which
    /// its parent reads in the next build. A build starts from the previous
    /// build's (its parents read them before their children overwrite
    /// them), so moved-over instances keep theirs.
    outputs: FastMap<u64, Values>,
    /// What this build overwrote in `outputs`: the value the previous build
    /// left (None: there was none), so what parents read stays known.
    overwritten: FastMap<u64, Option<Values>>,
    /// The outputs a seeded first build reads instead of `outputs` (the
    /// seed's); after a build, what it read.
    prev_outputs: Option<FastMap<u64, Values>>,
    /// The asset the tree was built from, for [`Cache::keep`].
    root: Option<Guid>,
    /// Every expanded file instance, in build order (depth first, so an
    /// instance's subtree is a run of nodes, files and records).
    /// Templates under hidden widgets are not expanded, so the shape
    /// follows what bindings show and node indices shift between builds;
    /// the key (see [`instance_key`]) finds an instance again.
    insts: Vec<Inst>,
    /// What the instances read of the previous build's sizes: (node, size,
    /// box start for bound-size containers). An instance's subtree reads
    /// are `Inst::reads`.
    reads: Vec<Read>,
    /// The previous build, by key: its record index and base node.
    prev_bases: FastMap<u64, (usize, usize)>,
    /// The previous build, which unchanged subtrees move over from, and per
    /// record whether its subtree would build the same again.
    prev: Option<Box<Tree>>,
    /// Bits by instance (see `on`).
    reusable: Vec<u64>,
    /// The previous build expanded other instances than the one before.
    reshaped: bool,
    /// This build evaluated or expanded an instance itself rather than
    /// moving it over from the previous one.
    built: bool,
    /// What it built differs from the previous build in something a box
    /// depends on (anything but alpha and tint), or is new.
    boxes_moved: bool,
    /// Per node: the node of the previous layout's tree (the one kept by
    /// [`Cache::keep`]) it is the same as, or [`NOT_KEPT`]; see
    /// [`Tree::previous`].
    prev_of: Vec<u32>,
    /// Per node: the node of the build before this one (`prev`) it is the
    /// same as, or [`NOT_KEPT`]: a size pass lays out only what moved.
    pass_of: Vec<u32>,
    /// What every template contributes, kept across builds and layouts.
    cache: Cache,
    /// Per instance key: its binding feed's fingerprint and what it
    /// evaluated to, this build and the previous one.
    evals: FastMap<u64, Eval>,
    prev_evals: FastMap<u64, Eval>,
    /// Which size pass this build is (0 the first), and `evals` of every
    /// pass of the previous layout of the same root: pass k of an
    /// unchanged screen evaluates what pass k did last time.
    pass: usize,
    layout_evals: Passes,
    /// Evaluations this layout made, over all its size passes, by what
    /// they read: instances of a template fed alike share one.
    same_evals: FastMap<u64, Arc<Expansion>>,
    /// Sample game data this layout gave out, by what names it: instances
    /// alike ask for the same, and naming and sampling a path builds
    /// strings.
    sampled_data: std::cell::RefCell<FastMap<u64, Option<DefaultValue>>>,
    /// See [`Options::stale`], sorted.
    stale: Vec<u64>,
    record_reads: bool,
    /// With `record_reads`: the paths a shared evaluation (`same_evals`)
    /// read, so instances that take it over count as readers too.
    same_reads: FastMap<u64, Vec<String>>,
    /// With [`Options::record_reads`]: per game-data path as the sampler
    /// was asked for it, the keys of the instances that read it.
    pub reads_by: std::cell::RefCell<FastMap<String, Vec<u64>>>,
    /// Per instance key (0 for the asset itself): what each binding read
    /// and wrote.
    pub traces: FastMap<u64, Arc<super::binding::Traces>>,
    /// Per instance key: the template property values its container passed
    /// in.
    inputs_in: FastMap<u64, Values>,
    /// Container instances a radial list placed with sample data; their
    /// items are not hidden as runtime-placed.
    pub placed: std::collections::HashSet<usize>,
}

/// What a build read of the layout of the build before it, at a node.
#[derive(Debug, Clone, Copy)]
enum Read {
    /// Its drawn size, and for a container a bound Size scales, where its
    /// box starts.
    Size(usize, Option<[f32; 2]>, Option<[f32; 2]>),
    /// Its transform to the root, which a Positioning component converts
    /// positions by (see `binding::POSITIONING`).
    At(usize, Option<[f32; 6]>),
}

impl Read {
    fn node(&self) -> usize {
        match *self {
            Read::Size(j, ..) | Read::At(j, _) => j,
        }
    }

    fn moved(self, f: impl Fn(usize) -> usize) -> Read {
        match self {
            Read::Size(j, size, min) => Read::Size(f(j), size, min),
            Read::At(j, at) => Read::At(f(j), at),
        }
    }
}

/// Template property values by UniqueId, shared between builds and with
/// the editor.
pub type Values = Arc<HashMap<u32, DefaultValue>>;

/// One instance's evaluation: its feed's fingerprint and the result.
type Eval = (u64, Arc<Expansion>);

/// What an instance of a template expands to from one evaluation: the
/// template's nodes with it applied (children relative to the instance's
/// base), and per container widget what it hands that container's
/// template, worked out when first expanded. Instances fed alike share
/// one, so most of a screen's repeated items apply and feed once.
struct Expansion {
    ev: super::binding::Evaluated,
    nodes: Vec<LayoutNode>,
    children: Vec<std::sync::OnceLock<ChildFeed>>,
}

impl Expansion {
    /// `nested`: the template is expanded by a container (not the tree's
    /// own asset, which keeps every loader variant for editing).
    fn new(
        ev: super::binding::Evaluated,
        t: &Template,
        asset: &Asset,
        inputs: &HashMap<u32, DefaultValue>,
        bindings: bool,
        nested: bool,
    ) -> Expansion {
        let (widgets, parsed) = (&t.parsed.0, &t.parsed.1);
        let table = &t.table;
        let mut nodes = parsed.clone();
        for &w in &table.idle_hidden {
            nodes[w].geometry.visible = false;
        }
        for (n, t) in nodes.iter_mut().zip(&ev.text) {
            n.text.clone_from(t);
        }
        for (n, t) in nodes.iter_mut().zip(&ev.loc) {
            n.loc.clone_from(t);
        }
        // Bound component properties change the layout components they
        // belong to: rebuild those from the stored overrides plus them.
        let mut touched: Vec<usize> = ev.components.keys().map(|k| k.0).collect();
        touched.sort_unstable();
        touched.dedup();
        for w in touched {
            let comps = table.components[w]
                .iter()
                .cloned()
                .map(|mut c| {
                    for ((_, _, prop), v) in ev
                        .components
                        .iter()
                        .filter(|(k, _)| k.0 == w && k.1 == c.uid)
                    {
                        c.overrides.retain(|(p, _)| p != prop);
                        c.overrides.push((*prop, v.clone()));
                    }
                    c
                })
                .filter_map(|c| LayoutComponent::from_component(&c))
                .collect();
            nodes[w].components = comps;
        }
        for (n, b) in nodes.iter_mut().zip(&ev.bound) {
            let b = *b;
            let g = &mut n.geometry;
            g.position = b.position.unwrap_or(g.position);
            g.pivot = b.pivot.unwrap_or(g.pivot);
            g.rotation = b.rotation.unwrap_or(g.rotation);
            g.scale = b.scale.unwrap_or(g.scale);
            g.alpha = b.alpha.unwrap_or(g.alpha);
            g.visible = b.visible.unwrap_or(g.visible);
            g.tint = b.tint.or(g.tint);
            n.bound = b;
        }
        // An enum widget shows only its live style's widgets.
        if bindings {
            let style = super::binding::current_style(asset, widgets, inputs);
            for n in &mut nodes {
                let e = n.geometry.enum_entry;
                if e != 0 && e != style {
                    n.geometry.visible = false;
                }
            }
        }
        // A GameObjectLoader instantiates the child made for its current
        // object's template; with no object (an empty pin slot, a tooltip
        // nothing hovers) it shows none. The loader's own file, opened for
        // editing, keeps all variants.
        // TODO(alex): a CurrentGameObject the editor could resolve picks no
        // variant yet; sample data never gives one.
        if bindings
            && nested
            && t.loader
            && let Some(root) = widgets.first()
        {
            for &c in &root.children {
                let has_template = widgets.get(c).is_some_and(|w| {
                    w.props.iter().any(|p| {
                        p.crc == PROP_GAME_OBJECT_TEMPLATE && matches!(p.value, Value::Ref(_))
                    })
                });
                if has_template && let Some(n) = nodes.get_mut(c) {
                    n.geometry.visible = false;
                }
            }
        }
        // Hover zones take their box from bindings the game runs once the
        // parent is laid out; at their stored place they stretch the parent
        // (the island name's wrapper grew 30 px down and centred its text
        // off the bar), so they do not count toward bounds. Those are
        // `InfoTipWidget`s, InfoTip template instances; InfoTipArea ones
        // (`hover_zones`) do not count either: the diplomacy confirm
        // button's sits at its group's width, which counting it widens,
        // 171 px a round without end, where the game shows a fixed panel.
        // TODO(alex): both inferred, not read from the exe; the InfoTip
        // template under other names (the construction menu's stamp
        // button) is left counting.
        for (k, w) in widgets.iter().enumerate() {
            if w.name.as_deref() == Some("InfoTipWidget") || table.hover_zones.contains(&k) {
                nodes[k].geometry.consider_size = false;
            }
        }
        Expansion {
            children: (0..widgets.len()).map(|_| Default::default()).collect(),
            ev,
            nodes,
        }
    }
}

/// What a container widget hands the template it expands, from its own
/// file's evaluation (see [`Parent`]).
struct ChildFeed {
    /// Values for the template's properties, sample values filled in.
    inputs: Values,
    context: HashMap<u32, String>,
    /// Template properties the parent feeds from game data, sorted.
    fed: Vec<u32>,
    /// Template properties the parent writes with any binding or stores:
    /// what is not in here stays empty in game (see `unfed` in `add`).
    /// Sorted.
    bound: Vec<u32>,
    /// UniqueIds of the instance's radial item groups (`WHEEL_GROUP`).
    wheel_groups: Vec<u32>,
    components: Vec<(u32, u32, DefaultValue)>,
    started: Vec<Guid>,
    /// [`values_hash`] of `inputs`, and [`context_hash`] of `context`
    /// mixed with `fed` and `bound`.
    hashes: (u64, u64),
}

/// Where a [`ChildFeed`] comes from: container widget `k` of `asset`,
/// expanding `tpl`.
struct ChildOf<'a> {
    asset: &'a Asset,
    k: usize,
    table: &'a super::binding::Prepared,
    ev: &'a super::binding::Evaluated,
    tpl: &'a Asset,
    child: &'a Template,
    context: &'a HashMap<u32, String>,
    /// Whether a property of `asset` stays empty in game (see `unfed` in
    /// `add`): what it feeds on is not fed either.
    unfed: &'a dyn Fn(u32) -> bool,
    sample: Option<Sampler>,
    /// `k` is the item prototype of a list the asset is (not idle).
    list_root: bool,
    shown: bool,
}

impl ChildFeed {
    fn new(c: ChildOf) -> ChildFeed {
        let ChildOf {
            asset,
            k,
            table,
            ev,
            tpl,
            ..
        } = c;
        // Values the instance passes into the template's properties:
        // stored overrides, then what the bindings computed. With sample
        // data on, inputs the game feeds drop their authored value so
        // the template samples them (a button's stored IsLocked=true
        // under a binding from the game's unlock state).
        let mut inputs: HashMap<u32, DefaultValue> = table.stored_inputs[k].clone();
        // Only flags: a stored size or text is a better stand-in than
        // the template default.
        if c.sample.is_some()
            && let Some(r) = ev.runtime.get(k)
        {
            let flag = |u: &u32| {
                tpl.properties()
                    .iter()
                    .any(|p| p.uid == *u && p.kind == PROPERTY_BOOL)
            };
            inputs.retain(|u, _| !(r.iter().any(|x| x.0 == *u) && flag(u)));
        }
        if let Some(b) = ev.inputs.get(k) {
            inputs.extend(b.iter().map(|(u, v)| (*u, v.clone())));
        }
        // A list's bindings size its items by writing the item
        // template's properties on the list root (`-Size` = the list's
        // length / TotalItemCount); the native list hands them on.
        if c.list_root
            && let Some(root) = ev.inputs.first()
        {
            inputs.extend(root.iter().map(|(u, v)| (*u, v.clone())));
        }
        // A root write to a property this file does not declare reaches
        // the root's child container whose template has it, as a root
        // component path reaches a child's component: the quest
        // objective's item writes its `-SizeInX` and `ObjectiveEntryData`
        // on its root for the objective template inside.
        let child_of_root = asset.widgets().get(k).is_some_and(|w| w.parent == Some(0));
        let via_root = |u: &u32| {
            child_of_root
                && tpl.properties().iter().any(|p| p.uid == *u)
                && !asset.properties().iter().any(|p| p.uid == *u)
        };
        if child_of_root && let Some(root) = ev.inputs.first() {
            for (u, v) in root.iter().filter(|(u, _)| via_root(u)) {
                inputs.entry(*u).or_insert_with(|| v.clone());
            }
        }
        let own_runtime = ev.runtime.get(k).into_iter().flatten();
        let root_runtime = ev
            .runtime
            .first()
            .into_iter()
            .flatten()
            .filter(|(u, _)| via_root(u) && k != 0);
        let runtime: Vec<&super::binding::GameFed> = own_runtime.chain(root_runtime).collect();
        let context: HashMap<u32, String> = runtime
            .iter()
            .copied()
            .filter(|(_, path)| path.first().is_some_and(|s| !(c.unfed)(s.1)))
            .map(|(uid, path)| (*uid, path_name(asset, path, c.context)))
            .collect();
        let mut components: Vec<(u32, u32, DefaultValue)> = table.components[k]
            .iter()
            .flat_map(|c| c.overrides.iter().map(|(p, v)| (c.uid, *p, v.clone())))
            .collect();
        let wheel_groups: Vec<u32> = table.components[k]
            .iter()
            .filter(|c| c.template == Some(Guid::from_const(WHEEL_GROUP)))
            .map(|c| c.uid)
            .collect();
        components.extend(
            ev.components
                .iter()
                .filter(|(key, _)| key.0 == k)
                .map(|(key, v)| (key.1, key.2, v.clone())),
        );
        let started = if c.shown {
            table
                .opening_nested
                .iter()
                .filter(|(w, _)| *w == k)
                .map(|(_, g)| *g)
                .collect()
        } else {
            Vec::new()
        };
        // In game a nested instance's properties are what its parent stores
        // and binds; only those it feeds from game data vary. Sample values
        // stand in for these alone: the rest keep the template default (the
        // construction menu's second round button leaves `-ShowDeco` off).
        let mut fed: Vec<u32> = runtime
            .iter()
            .copied()
            .filter(|(_, path)| path.first().is_none_or(|s| !(c.unfed)(s.1)))
            .map(|x| x.0)
            .collect();
        fed.sort_unstable();
        fed.dedup();
        // Written from game data the parent itself lacks: empty as well.
        let from_empty: Vec<u32> = runtime
            .iter()
            .copied()
            .filter(|(_, path)| path.first().is_some_and(|s| (c.unfed)(s.1)))
            .map(|x| x.0)
            .collect();
        // What the instance stores, whole or a field of it (a header's
        // `-Data.ID`, the text it shows).
        let stored: Vec<u32> = asset
            .property_values(&asset.widgets()[k])
            .into_iter()
            .filter_map(|(path, _)| path.first().copied())
            .collect();
        let mut bound: Vec<u32> = tpl
            .properties()
            .iter()
            .map(|p| p.uid)
            .filter(|&u| {
                table.writes_input(k, u)
                    || stored.contains(&u)
                    || (via_root(&u) && table.writes_input(0, u))
            })
            .filter(|u| !from_empty.contains(u))
            .collect();
        bound.sort_unstable();
        let mut inputs = c.child.with_samples(tpl, c.sample, inputs, Some(&fed));
        // A fed input is the game data its path names: sampled by that
        // name, not the bare property name (an objective's text atom reads
        // `-Data.Text`, fed from `…ObjectiveIconTitle.TextData.Text`).
        if let Some(sample) = c.sample {
            for p in tpl.properties() {
                let Some(name) = context.get(&p.uid) else {
                    continue;
                };
                if fed.binary_search(&p.uid).is_err()
                    || ev.inputs.get(k).is_some_and(|b| b.contains_key(&p.uid))
                {
                    continue;
                }
                if let Some(v) = sample(name, p.kind, tpl.property_default(p)) {
                    inputs.insert(p.uid, v);
                }
            }
        }
        let fed_hash = {
            use std::hash::{Hash, Hasher};
            let mut h = hasher();
            (&fed, &bound).hash(&mut h);
            h.finish()
        };
        ChildFeed {
            hashes: (values_hash(&inputs), context_hash(&context) ^ fed_hash),
            inputs: Arc::new(inputs),
            context,
            fed,
            bound,
            wheel_groups,
            components,
            started,
        }
    }
}

/// One expanded file instance of a build. `end`, `files_end`, `last` and
/// `reads.1` are one past its subtree's nodes, files, records and reads.
#[derive(Debug, Clone, Copy, Default)]
struct Inst {
    key: u64,
    base: usize,
    end: usize,
    file: usize,
    files_end: usize,
    last: usize,
    reads: (usize, usize),
    /// The container node it expands under.
    container: Option<usize>,
    /// Evaluated anew rather than as in the previous build: its outputs
    /// may differ from what its parent read then.
    fresh: bool,
    /// Its parent's bindings read its outputs.
    outputs_read: bool,
    /// Under a hidden container: [`incoming`] of what it was built from
    /// (else 0), and once collapsed (see [`Tree::collapse`]) the box its
    /// container keeps.
    incoming: u64,
    collapsed: Option<Rect>,
    /// See [`Parent::sized`].
    sized: bool,
}
/// Per size pass, per instance key: what it evaluated.
type Passes = Vec<FastMap<u64, Eval>>;

/// No node of the previous layout ([`Tree::previous`]).
pub const NOT_KEPT: u32 = u32::MAX;

/// Whether two nodes are the same, but for which nodes their children are.
fn same_node(a: &LayoutNode, b: &LayoutNode) -> bool {
    boxes_alike(a, b)
        && a.geometry.alpha == b.geometry.alpha
        && a.geometry.tint == b.geometry.tint
        && a.bound.alpha == b.bound.alpha
        && a.bound.tint == b.bound.tint
}

/// Whether two nodes lay out the same box: all but alpha and tint, which
/// only colour what is drawn (a hover's highlight).
fn boxes_alike(a: &LayoutNode, b: &LayoutNode) -> bool {
    let geometry = |g: &Geometry| Geometry {
        alpha: 0.,
        tint: None,
        ..g.clone()
    };
    let bound = |b: &super::binding::Bound| super::binding::Bound {
        alpha: None,
        tint: None,
        ..*b
    };
    geometry(&a.geometry) == geometry(&b.geometry)
        && bound(&a.bound) == bound(&b.bound)
        && a.intrinsic == b.intrinsic
        && a.origin == b.origin
        && a.size == b.size
        && a.children.len() == b.children.len()
        && a.components == b.components
        && a.text == b.text
        && a.item == b.item
}

/// Frees `x` on another thread. A tree or a layout's evaluations hold
/// thousands of allocations (the transfer screen's took 190 ms to free),
/// none of which a relayout needs to wait for.
pub fn drop_later<T: Send + 'static>(x: T) {
    type Garbage = Box<dyn Send>;
    static BIN: std::sync::OnceLock<std::sync::Mutex<std::sync::mpsc::Sender<Garbage>>> =
        std::sync::OnceLock::new();
    let bin = BIN.get_or_init(|| {
        let (tx, rx) = std::sync::mpsc::channel::<Garbage>();
        std::thread::spawn(move || rx.into_iter().for_each(drop));
        std::sync::Mutex::new(tx)
    });
    if let Ok(bin) = bin.lock() {
        // A closed bin frees it here instead.
        let _ = bin.send(Box::new(x));
    }
}

/// A container instance's key: its parent instance's and where in that
/// file it sits (node offset from the file's base, list copies included).
/// The tree's own asset is 0.
fn instance_key(parent: u64, offset: usize) -> u64 {
    // splitmix64 over both.
    let mut z = parent ^ (offset as u64).wrapping_add(0x9e37_79b9_7f4a_7c15);
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

/// What layouts keep between them: per template (by guid), what building
/// reads from it alone, and the previous layout's evaluations per root.
/// Templates come from the install and never change; the root being built
/// is dropped from the per-template maps first, as the editor changes it.
#[derive(Default)]
pub struct Cache {
    /// Per template, what building reads from it alone (a template may
    /// expand hundreds of times).
    templates: FastMap<Guid, Arc<Template>>,
    /// Per root, most recent last: every size pass's evaluations, by
    /// instance key, and where its sizes settled.
    layouts: Vec<(Option<Guid>, Passes, Option<Seed>)>,
    /// Each root as last laid out: unchanged (a hover, a clip's time), its
    /// own file's parse and evaluation hold like a template's.
    roots: FastMap<Guid, Vec<u8>>,
    /// The last tree kept from a layout that did not settle (it seeds
    /// nothing), for [`Cache::tree`].
    unsettled: Option<Box<Tree>>,
}

/// What the last build of a layout that settled was built from: the
/// build before it's sizes, box starts and outputs, and its (key, base)
/// per instance. The next layout of the same root starts there instead
/// of from no sizes: an unchanged screen settles in one build, an edited
/// one in a few, where a fresh one takes up to [`SIZE_PASSES`].
struct Seed {
    sizes: Vec<[f32; 2]>,
    mins: Vec<[f32; 2]>,
    transforms: Vec<[f32; 6]>,
    outputs: FastMap<u64, Values>,
    bases: FastMap<u64, (usize, usize)>,
    shape: Vec<(u64, usize)>,
    /// The settled tree itself, handed back by [`Cache::keep`], and the
    /// options it was built with. A first build from the seed with the
    /// same options moves unchanged subtrees over from it: they were
    /// built from these very sizes and outputs. Other options (a clip's
    /// time, preview values) can change what a subtree builds below
    /// where its feed shows it.
    tree: Option<Box<Tree>>,
    opts: Options,
    /// The layout of that tree, for the next to take over
    /// ([`evaluate_since`]).
    laid: Option<Laid>,
}

/// Screens whose evaluations a [`Cache`] keeps: the open file and the game
/// view's HUD (seven) with room to switch back and forth.
const CACHED_LAYOUTS: usize = 16;

impl Cache {
    /// Takes back a tree [`Tree::build_cached`] returned, as built (callers
    /// that changed node sizes put them back first), for the next layout
    /// of the same root to start from.
    pub fn keep(&mut self, tree: Tree) {
        let root = tree.root;
        drop_later(self.unsettled.take());
        match self.layouts.iter_mut().find(|(r, ..)| *r == root) {
            Some((.., Some(seed))) => seed.tree = Some(Box::new(tree)),
            _ => self.unsettled = Some(Box::new(tree)),
        }
    }

    /// The last tree [`Cache::keep`] took back for `root`, for what the
    /// editor shows of it on demand (a selected instance's bindings).
    pub fn tree(&self, root: Option<Guid>) -> Option<&Tree> {
        let settled = self
            .layouts
            .iter()
            .find(|(r, ..)| *r == root)
            .and_then(|(.., seed)| seed.as_ref()?.tree.as_deref());
        settled.or(self.unsettled.as_deref().filter(|t| t.root == root))
    }

    /// The widgets of a file the last layout expanded (the root's too).
    pub fn parsed(&self, g: Guid) -> Option<Arc<Parsed>> {
        Some(self.templates.get(&g)?.parsed.clone())
    }

    /// Drops everything kept for root `g`: after a structural edit (a
    /// widget added, removed or moved) nodes no longer line up with the
    /// last layout's, and identical-looking stack items would take each
    /// other's places.
    /// Drops what was read from template `g` alone (its parse and the
    /// template inputs it samples by name), keeping the layouts built
    /// with it: instances of it must be rebuilt
    /// ([`Options::stale`]).
    pub fn forget_template(&mut self, g: Guid) {
        self.templates.remove(&g);
    }

    pub fn forget(&mut self, g: Option<Guid>) {
        if let Some(g) = g {
            self.templates.remove(&g);
            self.roots.remove(&g);
        }
        self.layouts.retain(|(r, ..)| *r != g);
        self.unsettled = None;
    }

    /// Reads, on every core, what building takes from each of `templates`
    /// alone (see [`Template`]) where no layout has yet: a screen's first
    /// build then only expands them. `load` finds other assets as
    /// building's loader does.
    pub fn warm(
        &mut self,
        templates: &[Arc<Asset>],
        load: &(dyn Fn(Guid) -> Option<Arc<Asset>> + Sync),
    ) {
        use rayon::prelude::*;
        let todo: Vec<(Guid, &Arc<Asset>)> = templates
            .iter()
            .filter_map(|a| Some((a.guid()?, a)))
            .filter(|(g, _)| !self.templates.contains_key(g))
            .collect();
        let read: Vec<_> = todo
            .par_iter()
            .filter_map(|&(g, a)| {
                if a.widgets().is_empty() {
                    return None;
                }
                Some((g, Arc::new(Template::read(a, &mut |g| load(g)))))
            })
            .collect();
        self.templates.extend(read);
    }

    /// What building reads from `asset` alone, read on first use.
    fn template(
        &mut self,
        asset: &Asset,
        load: &mut dyn FnMut(Guid) -> Option<Arc<Asset>>,
    ) -> Arc<Template> {
        match asset.guid() {
            Some(g) => match self.templates.get(&g) {
                Some(t) => t.clone(),
                None => {
                    let t = Arc::new(Template::read(asset, load));
                    self.templates.insert(g, t.clone());
                    t
                }
            },
            None => Arc::new(Template::read(asset, load)),
        }
    }
}

/// What building reads from one template alone, whatever instance expands
/// it: once per template, not per instance and size pass.
struct Template {
    parsed: Arc<Parsed>,
    /// What binding evaluation and container expansion read.
    table: Arc<super::binding::Prepared>,
    clips: Arc<Vec<super::animation::Clip>>,
    /// The opening clips' end state, when the parent Starts none of its own,
    /// and its hash.
    settled: Arc<Settled>,
    settled_hash: u64,
    /// Image and video sizes per widget.
    media: Vec<Option<[f32; 2]>>,
    /// ReferenceSize, and whether the template uses it as its box.
    reference: Option<[f32; 2]>,
    fixed: bool,
    /// The item prototype of a list (see [`list_prototype`]); whether it
    /// is radial, and whether a scrollable list (else a grid).
    list: Option<usize>,
    radial: bool,
    stacks: bool,
    /// A GameObjectLoader: shows the child for its current object only.
    loader: bool,
    /// What the sampler gives each property (one sampler per process).
    sampled: std::sync::OnceLock<Vec<(u32, DefaultValue)>>,
}

impl Template {
    /// `inputs` with sample values where nothing passes one in. What the
    /// sampler gives each property depends on the template alone.
    fn with_samples(
        &self,
        asset: &Asset,
        sample: Option<Sampler>,
        mut inputs: HashMap<u32, DefaultValue>,
        only: Option<&[u32]>,
    ) -> HashMap<u32, DefaultValue> {
        let Some(sample) = sample else {
            return inputs;
        };
        let sampled = self.sampled.get_or_init(|| {
            asset
                .properties()
                .iter()
                .filter_map(|p| {
                    let v = sample(asset.property_name(p), p.kind, asset.property_default(p))?;
                    Some((p.uid, v))
                })
                .collect()
        });
        for (uid, v) in sampled {
            if only.is_none_or(|o| o.contains(uid)) {
                inputs.entry(*uid).or_insert_with(|| v.clone());
            }
        }
        inputs
    }

    fn read(asset: &Asset, load: &mut dyn FnMut(Guid) -> Option<Arc<Asset>>) -> Template {
        let widgets = asset.widgets();
        let nodes = layout_nodes(asset, &widgets);
        let parsed = Arc::new((widgets.to_vec(), nodes));
        let widgets = &parsed.0;
        let table = prepare_template(asset, widgets, load);
        let clips = Arc::new(asset.clips());
        // Clips opening Starts, in the order they start (later writes win:
        // the main title Starts Anim_Default, then Anim_In), after clips
        // only their name says open.
        let settled = Arc::new(opening_end(
            &clips,
            &table.opening_clips,
            &table.state_clips,
            &fired_state_clips(asset, &table.state_clips, &HashMap::new(), None),
            &[],
        ));
        let media = widgets
            .iter()
            .map(|w| {
                video_size(w, load).or_else(|| {
                    asset
                        .widget_image(w)
                        .and_then(|im| im.asset)
                        .and_then(|r| load(r.id))
                        .and_then(|a| super::image_info(&a))
                        .map(|info| info.size)
                })
            })
            .collect();
        let list = list_prototype(asset, widgets, load);
        Template {
            reference: asset.reference_size(),
            fixed: uses_reference_size(asset),
            settled_hash: {
                use std::hash::{Hash, Hasher};
                let mut h = hasher();
                settled[..].hash(&mut h);
                h.finish()
            },
            radial: list.is_some() && derives_from(asset, load, RADIAL_WIDGET_TYPE),
            stacks: list.is_some() && derives_from(asset, load, LIST_TYPE),
            loader: derives_from(asset, load, GAME_OBJECT_LOADER),
            list,
            parsed,
            table,
            clips,
            settled,
            media,
            sampled: std::sync::OnceLock::new(),
        }
    }
}

/// The end state of a file's opening clips: those its own bindings Start
/// as the screen opens (`own`), then those its parent Starts (by id),
/// after clips only their name says open; later writes win.
/// The state-started clips (see `Bindings::clips_started_by_state`) that
/// fire as the instance opens: a trigger property starts out true, as
/// evaluated (`values`, once there is an evaluation) or passed in.
fn fired_state_clips(
    asset: &Asset,
    state: &[(usize, Vec<usize>)],
    inputs: &HashMap<u32, DefaultValue>,
    values: Option<&HashMap<u32, DefaultValue>>,
) -> Vec<usize> {
    let props = asset.properties();
    let on = |i: &usize| {
        props.get(*i).is_some_and(|p| {
            let v = values
                .and_then(|m| m.get(&p.uid))
                .or_else(|| inputs.get(&p.uid))
                .cloned()
                .unwrap_or_else(|| asset.property_default(p).clone());
            matches!(v, DefaultValue::Bool(b) if b != 0)
        })
    };
    state
        .iter()
        .filter(|(_, triggers)| triggers.iter().any(on))
        .map(|(c, _)| *c)
        .collect()
}

fn opening_end(
    clips: &[super::animation::Clip],
    own: &[usize],
    state: &[(usize, Vec<usize>)],
    fired: &[usize],
    started: &[Guid],
) -> Settled {
    // Named opening clips that state starts play only when it fires.
    let quiet: Vec<usize> = state
        .iter()
        .map(|(c, _)| *c)
        .filter(|c| !fired.contains(c))
        .collect();
    let triggered: Vec<usize> =
        own.iter()
            .chain(fired)
            .copied()
            .chain((0..clips.len()).filter(|&i| {
                clips[i].id.is_some_and(|g| started.contains(&g)) && !own.contains(&i)
            }))
            .collect();
    (0..clips.len())
        .filter(|&i| clips[i].opens() && !triggered.contains(&i) && !quiet.contains(&i))
        .chain(triggered.iter().copied())
        .filter_map(|i| clips.get(i))
        .flat_map(|c| c.sample(c.duration()))
        .collect()
}

/// What binding evaluation and container expansion read from a template
/// itself, once per template.
fn prepare_template(
    asset: &Asset,
    widgets: &[Widget],
    load: &mut dyn FnMut(Guid) -> Option<Arc<Asset>>,
) -> Arc<super::binding::Prepared> {
    let p = super::binding::Prepared::new(asset, widgets);
    let bars = (0..widgets.len())
        .filter(|&k| {
            p.templates[k]
                .and_then(&mut *load)
                .is_some_and(|t| derives_from(&t, load, SCROLLBAR_WIDGET_TYPE))
        })
        .collect();
    let zones = (0..widgets.len())
        .filter(|&k| {
            p.templates[k].and_then(&mut *load).is_some_and(|t| {
                let root = t.widgets();
                root.first().is_some_and(|r| {
                    t.components(r)
                        .iter()
                        .any(|c| c.template == Some(INFOTIP_AREA))
                })
            })
        })
        .collect();
    // An InputText's caret and selection (found by name, as the widget type's
    // `ui_textInput` is) show only while it has the focus, which nothing has
    // in a screen at rest: the governors bar's island name drew a caret.
    let mut hidden: Vec<usize> = bars;
    if derives_from(asset, load, INPUT_TEXT_WIDGET_TYPE) {
        hidden.extend(widgets.iter().enumerate().filter_map(|(k, w)| {
            matches!(w.name.as_deref(), Some("ui_caret" | "ui_selection")).then_some(k)
        }));
    }
    let mut p = p.start_hidden(hidden);
    p.hover_zones = zones;
    Arc::new(p)
}

/// A template's opening clips at their end: (slot, value) writes.
type Settled = Vec<(usize, DefaultValue)>;

/// Most builds after the first that read the previous layout's sizes.
/// Sizes feed bindings that size other widgets (a list's length from its
/// column width, its items from its length), so it takes rounds to settle,
/// as the game converges over frames; building stops once the sizes no
/// longer change. The construction menu settles in nine: its pin slots'
/// drop zones size a scaling helper from the slot and the slot from them,
/// and its popup anchors (Positioning outputs, screen positions from the
/// pass before's transforms) trail the transforms by a pass. A layout
/// that has not settled seeds nothing, so every later relayout of the
/// screen would start over: the cap leaves room above that.
const SIZE_PASSES: usize = 12;

/// What a container instance hands the template it expands (all empty for
/// the tree's own asset).
#[derive(Default, Clone, Copy)]
struct Parent<'a> {
    /// The container's node.
    instance: Option<usize>,
    /// Values for the template's properties, sample values filled in.
    inputs: Option<&'a Values>,
    /// [`values_hash`] of `inputs` and [`context_hash`] of `context`.
    hashes: (u64, u64),
    /// Writes its file's bindings make into the template's widgets.
    writes: &'a [super::binding::NestedWrite],
    /// A radial list item's slice (angle, angle size, position): what the
    /// list writes into the template root's radial item component.
    slice: Option<(f32, f32, [f32; 2])>,
    /// A list item's position in its list (the prototype is 0).
    index: Option<u32>,
    /// The nearest list item this instance is in, or is: the list's
    /// DataSource by name and the item's position. Sample data inside
    /// tells the items apart by it (the minimap ring's icons).
    item: Option<(&'a str, u32)>,
    /// Template properties the parent feeds from game data, named by the
    /// data path they come from (`ObjectiveEntryData.ObjectiveIconTitle`),
    /// so sample data inside sees what a generic `-Data` stands for.
    context: Option<&'a HashMap<u32, String>>,
    /// Template properties the parent feeds from game data (sorted); the
    /// tree's own asset has none.
    fed: &'a [u32],
    /// Template properties the parent writes with any binding or stores
    /// (sorted).
    bound: &'a [u32],
    /// UniqueIds of the container's radial item group components.
    wheel_groups: &'a [u32],
    /// The container's component values (component UniqueId, property,
    /// value): stored overrides, then its file's bindings' writes.
    components: &'a [(u32, u32, DefaultValue)],
    /// How many sample-stamped lists this expansion sits in.
    lists: u8,
    /// Clips of this template its parent Starts as the screen opens.
    started: &'a [Guid],
    /// See [`instance_key`].
    key: u64,
    /// The parent's bindings read the container's box, so what it spans
    /// counts even under a hidden widget.
    sized: bool,
    /// The container is hidden: expanded only for its box or outputs.
    hidden: bool,
    /// The parent found it reusable as it was (see `add`); nothing else
    /// is filled in.
    reuse: bool,
}

/// What a container hands its template, hashed (for a hidden one: what
/// its collapsed subtree was built from). Never 0.
fn incoming(p: &Parent) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = hasher();
    // Maps hash order-independently: a sum of per-entry hashes.
    p.hashes.hash(&mut h);
    p.writes.hash(&mut h);
    p.slice
        .map(|(a, b, c)| (a.to_bits(), b.to_bits(), c.map(f32::to_bits)))
        .hash(&mut h);
    p.index.hash(&mut h);
    p.components.hash(&mut h);
    (p.lists, p.started, p.sized).hash(&mut h);
    h.finish() | 1
}

/// Template property values, hashed independently of the map's order (a
/// sum of per-entry hashes).
pub(super) fn values_hash(m: &HashMap<u32, DefaultValue>) -> u64 {
    use std::hash::{Hash, Hasher};
    m.iter().fold(0u64, |acc, e| {
        let mut h = hasher();
        e.hash(&mut h);
        acc.wrapping_add(h.finish())
    })
}

/// The game-data names a container hands its template, hashed.
fn context_hash(context: &HashMap<u32, String>) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut names: Vec<_> = context.iter().collect();
    names.sort_unstable();
    let mut h = hasher();
    names.hash(&mut h);
    h.finish()
}

/// A data path by name: template properties, then game-object fields;
/// a template property the parent feeds from game data takes its name.
fn path_name(asset: &Asset, path: &[(u32, u32)], context: &HashMap<u32, String>) -> String {
    path.iter()
        .filter_map(|&(_, id)| {
            if let Some(c) = context.get(&id) {
                return Some(c.clone());
            }
            super::tables::data_field(id)
                .map(|f| f.1.to_owned())
                .or_else(|| {
                    let p = asset.properties().iter().find(|p| p.uid == id)?;
                    Some(asset.property_name(p).to_owned())
                })
        })
        .collect::<Vec<_>>()
        .join(".")
}

/// A template's widgets and their unevaluated layout nodes.
pub type Parsed = (Vec<Widget>, Vec<LayoutNode>);

/// Template property values to show instead of the defaults and of what a
/// container instance passes in, as game data would set them: by the
/// file's guid (the root object's id), then custom property UniqueId.
pub type Preview = HashMap<Guid, HashMap<u32, DefaultValue>>;

/// A clip held at its end on one instance of a template: an interaction
/// state such as `Enter` (hover) or `Press`. `file` None and `instance`
/// None mean the asset the tree is built from; else `instance` is the
/// instance key ([`Tree::instance_keys`]) of the container widget that
/// expands template `file`. Keys, unlike node indices, hold from one
/// layout to the next.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Sim {
    pub file: Option<Guid>,
    pub instance: Option<u64>,
    pub clip: usize,
}

/// What [`Tree::build_opts`] evaluates.
#[derive(Debug, Clone, Default)]
pub struct Options {
    pub bindings: bool,
    pub play: Option<Play>,
    pub preview: Preview,
    /// Every file's opening clips ([`super::animation::Clip::opens`]) at
    /// their end: the screen as it looks once opened, not its first frame
    /// (the main title stores its content at alpha 0 and fades it in).
    pub shown: bool,
    pub sims: Vec<Sim>,
    /// Fills template properties nothing else sets with a plausible game
    /// value: (name, CustomPropertyType, default) → value.
    pub sample: Option<Sampler>,
    /// Which version of what `sample` answers: the editor's hand-set game
    /// data changes it without changing the function.
    pub sample_generation: u64,
    /// Instance keys whose game data answers differently than in the last
    /// build of the same root although the sampler did not change (the
    /// editor's flag probe flips one flag at a time): they are rebuilt and
    /// evaluated again, the rest of the tree comes over as it was.
    pub stale: Vec<u64>,
    /// Record which instances read which game-data path
    /// ([`Tree::reads_by`]).
    pub record_reads: bool,
}

impl Options {
    /// Where trees built with `self` and with `o` can differ below what an
    /// instance's feed shows: the templates whose clip or preview values
    /// changed, and the instance keys whose simulated state did. None:
    /// anywhere (bindings, opening state or sample data changed). Samplers
    /// compare by address, which may differ for the same function: then
    /// anywhere, which only costs reuse.
    fn changes(&self, o: &Options) -> Option<(Vec<Guid>, Vec<u64>)> {
        let same_sampler = match (self.sample, o.sample) {
            (Some(a), Some(b)) => std::ptr::fn_addr_eq(a, b),
            (a, b) => a.is_none() && b.is_none(),
        };
        if self.bindings != o.bindings
            || self.shown != o.shown
            || !same_sampler
            || self.sample_generation != o.sample_generation
        {
            return None;
        }
        let mut files = Vec::new();
        if self.play != o.play {
            files.extend([self.play, o.play].iter().flatten().filter_map(|p| p.file));
        }
        let keys = self.preview.keys().chain(o.preview.keys());
        files.extend(
            keys.filter(|g| self.preview.get(g) != o.preview.get(g))
                .copied(),
        );
        // The root's own file is rebuilt anyway.
        let moved = |a: &[Sim], b: &[Sim]| {
            a.iter()
                .filter(|s| !b.contains(s))
                .copied()
                .collect::<Vec<_>>()
        };
        let containers = moved(&self.sims, &o.sims)
            .into_iter()
            .chain(moved(&o.sims, &self.sims))
            .filter_map(|s| s.instance)
            .chain(o.stale.iter().copied())
            .collect();
        Some((files, containers))
    }
}

/// Breaks `text` at spaces into lines no wider than `width`; a word wider
/// than that keeps a line to itself. Existing line breaks stay.
fn wrap(text: &str, width: f32, measure: &mut dyn FnMut(&str) -> f32) -> Vec<String> {
    let mut lines = Vec::new();
    for para in text.lines() {
        let mut line = String::new();
        for word in para.split(' ') {
            let candidate = if line.is_empty() {
                word.to_owned()
            } else {
                format!("{line} {word}")
            };
            if !line.is_empty() && measure(&candidate) > width {
                lines.push(std::mem::replace(&mut line, word.to_owned()));
            } else {
                line = candidate;
            }
        }
        lines.push(line);
    }
    lines
}

/// One label line in ems, from the DisplayedSizes shipped before the
/// 2026-11-05 patch zeroed them. The editor takes a taller font's own
/// metrics over it (Albertus Nova titles are 1.442).
/// TODO(alex): the body font's metrics say 1.227; how the game rounds is
/// not read from the exe.
pub const LINE_EM: f32 = 1.25;

/// A label's lines and box for `text` in a TextPlane of `plane` (stored or
/// bound): HorizontalOverflow Wrap (1) breaks at spaces inside the plane's
/// width, MaxLineCount cuts, and each axis is sized per TextBoxSizing (0 the
/// plane, 1 the text, 2 the larger, 3 the smaller). `line` is one line's
/// height in pixels at LineSpacing 1 (see `LINE_EM`); `measure` is one
/// line's width in pixels.
/// TODO(alex): break rules are inferred from shipped DisplayedSizes, not
/// read from the exe.
pub fn label_box(
    l: &super::LabelText,
    text: &str,
    line: f32,
    plane: [f32; 2],
    measure: &mut dyn FnMut(&str) -> f32,
) -> (Vec<String>, [f32; 2]) {
    let mut lines = if l.overflow[0] == 1 && plane[0] > 0. {
        wrap(text, plane[0], measure)
    } else {
        text.lines().map(str::to_owned).collect()
    };
    if l.max_lines >= 1. {
        lines.truncate(l.max_lines as usize);
    }
    let line_height = line * l.line_spacing.max(0.1);
    let content = [
        lines.iter().map(|t| measure(t)).fold(0., f32::max),
        lines.len().max(1) as f32 * line_height,
    ];
    let size = [0, 1].map(|a| match l.sizing[a] {
        0 if plane[a] > 0. => plane[a],
        2 => content[a].max(plane[a]),
        3 if plane[a] > 0. => content[a].min(plane[a]),
        _ => content[a],
    });
    (lines, size)
}

/// Where a label's box of `size` starts in its TextPlane: by alignment,
/// larger text overflows (centred grows both ways, right or bottom grows
/// back past the origin; the minimap's lode counts sit centred on a 0×0
/// plane) and smaller text moves into the plane.
pub fn label_origin(l: &super::LabelText, plane: [f32; 2], size: [f32; 2]) -> [f32; 2] {
    [0, 1].map(|a| {
        let f = match l.alignment[a] {
            1 => 0.5,
            // Right on x (3 is Justify); Baseline / Bottom on y.
            2 => 1.,
            3 if a == 1 => 1.,
            _ => 0.,
        };
        (plane[a] - size[a]) * f
    })
}

/// One line's width in pixels for a label of TextStyle `style` (its font,
/// size and letter spacing); None when the font is unknown.
pub type Measure<'a> = &'a mut dyn FnMut(Guid, &str) -> Option<f32>;

/// See [`Options::sample`]. Called with (name, PropertyType code, default);
/// game-object fields have no default and come with `DefaultValue::None`.
pub type Sampler = fn(&str, u32, &DefaultValue) -> Option<DefaultValue>;

/// A clip being played: the file it belongs to (None: the asset the tree is
/// built from; else a template's guid), its index in [`Asset::clips`], and
/// the time in seconds.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Play {
    pub file: Option<Guid>,
    pub clip: usize,
    pub time: f32,
}

/// Templates nest (a container of containers); deeper chains are cut, which
/// also stops cycles.
const MAX_TEMPLATE_DEPTH: usize = 12;
const PROP_USES_REFERENCE_SIZE: u32 = name_crc(b"ForceReferenceSize");

impl Tree {
    /// Builds the tree of `asset` (node 0 = its root). `load` reads a
    /// generated asset by guid: container templates and ImageAssets (for
    /// image sizes). Labels take their `DisplayedSize`; callers may fill in
    /// other intrinsic sizes before `evaluate`.
    pub fn build(asset: &Asset, load: &mut dyn FnMut(Guid) -> Option<Arc<Asset>>) -> Tree {
        Tree::build_with(asset, load, true)
    }

    /// [`Tree::build`], with the templates' data bindings evaluated over the
    /// stored geometry (`bindings`) or not (the values as stored).
    pub fn build_with(
        asset: &Asset,
        load: &mut dyn FnMut(Guid) -> Option<Arc<Asset>>,
        bindings: bool,
    ) -> Tree {
        Tree::build_opts(
            asset,
            load,
            &mut |_, _| None,
            &Options {
                bindings,
                ..Options::default()
            },
        )
    }

    /// [`Tree::build`] with every option: bindings evaluated or not, one
    /// timeline clip sampled (its tracks write through the binding slots of
    /// the file that owns it, every instance of it when it is a template),
    /// and preview values for template properties.
    pub fn build_opts(
        asset: &Asset,
        load: &mut dyn FnMut(Guid) -> Option<Arc<Asset>>,
        measure: Measure,
        opts: &Options,
    ) -> Tree {
        Tree::build_cached(asset, load, measure, opts, &mut Cache::default())
    }

    /// [`Tree::build_opts`] keeping per-template work and evaluations in
    /// `cache` for the next layout: an editor relayout after a small edit
    /// re-evaluates only what the edit reached.
    pub fn build_cached(
        asset: &Asset,
        load: &mut dyn FnMut(Guid) -> Option<Arc<Asset>>,
        measure: Measure,
        opts: &Options,
        cache: &mut Cache,
    ) -> Tree {
        let root = asset.guid();
        let unchanged =
            root.is_some_and(|g| cache.roots.get(&g).is_some_and(|b| b[..] == *asset.bytes()));
        if let Some(g) = root.filter(|_| !unchanged) {
            cache.templates.remove(&g);
            cache.roots.insert(g, asset.bytes().to_vec());
        }
        let reference = asset.reference_size();
        let (layout_evals, seed) = match cache.layouts.iter().position(|(r, ..)| *r == root) {
            Some(i) => {
                let (_, passes, seed) = cache.layouts.remove(i);
                (passes, seed.filter(|_| opts.bindings))
            }
            None => (Vec::new(), None),
        };
        // The last layout's evaluations hold only under the options they
        // were made with: after hand-set game data, sampling or bindings
        // change, a nested instance's feed can match while what it reads
        // from game data does not (round 9: the minimap kept its tutorial
        // content after Reset all). `prev_evals` is cleared the same way.
        let layout_evals = if seed
            .as_ref()
            .is_some_and(|s| s.opts.changes(opts).is_some())
        {
            layout_evals
        } else {
            drop_later(layout_evals);
            Vec::new()
        };
        let mut passes: Passes = Vec::new();
        let mut settled: Option<Seed> = None;
        let mut stale = opts.stale.clone();
        stale.sort_unstable();
        let mut t = Tree {
            bindings: opts.bindings,
            play: opts.play,
            preview: opts.preview.clone(),
            shown: opts.shown,
            sims: opts.sims.clone(),
            sample: opts.sample,
            stale,
            record_reads: opts.record_reads,
            root,
            cache: std::mem::take(cache),
            ..Tree::default()
        };
        let mut last_laid: Option<Laid> = None;
        let shape = match seed {
            Some(mut seed) => {
                last_laid = seed.laid.take();
                t.sizes = seed.sizes;
                t.transforms = seed.transforms;
                t.mins = seed.mins;
                t.prev_outputs = Some(seed.outputs);
                t.prev_bases = seed.bases;
                let changes = seed.opts.changes(opts);
                let incompatible = changes.is_none();
                if incompatible {
                    drop_later(seed.tree.take());
                }
                if let Some(tree) = seed.tree
                    && let Some((mut files, containers)) = changes
                {
                    // An edited root file is not what the old tree was
                    // built from: a widget moved in a stack or a new child
                    // changes the instances of the root file too.
                    if !unchanged {
                        files.extend(root);
                    }
                    // An instance of a changed template, or a container whose
                    // state changed, and every instance around it, rebuild.
                    let file_of = |d: &Inst| match d.file {
                        0 => tree.root,
                        f => tree.templates[f - 1].guid(),
                    };
                    let mut hit: Vec<usize> = tree
                        .insts
                        .iter()
                        .filter(|d| {
                            file_of(d).is_some_and(|g| files.contains(&g))
                                || containers.contains(&d.key)
                        })
                        .map(|d| d.base)
                        .collect();
                    hit.sort_unstable();
                    t.reusable = bits(tree.insts.iter().map(|r| {
                        let first = hit.partition_point(|&b| b < r.base);
                        hit.get(first).is_none_or(|&b| b >= r.end)
                    }));
                    t.reserve_like(&tree);
                    // The anchor of `previous`: the last layout's tree.
                    let mut tree = tree;
                    tree.prev_of = (0..tree.nodes.len() as u32).collect();
                    t.take_values(&mut tree);
                    t.prev = Some(tree);
                }
                // The settled build's evaluations, but the root's own file,
                // which is the one the editor changes.
                t.prev_evals = layout_evals.last().cloned().unwrap_or_default();
                if !unchanged {
                    t.prev_evals.remove(&0);
                }
                // Other options (hand-set game data, sampling, bindings) can
                // change what any evaluation read: none of them holds.
                if incompatible {
                    t.prev_evals.clear();
                }
                Some(seed.shape)
            }
            None => None,
        };
        t.layout_evals = layout_evals;
        let kept = t.prev.is_some();
        t.add(asset, 0, load, 0, Parent::default());
        t.prune_values();
        drop_later(t.prev.take());
        t.reusable = Vec::new();
        if let Some(root) = t.nodes.first_mut() {
            root.size = reference;
        }
        // Built from a seed: settled if built from it in the same shape.
        t.reshaped = shape.is_some_and(|s| !t.insts.iter().map(|d| (d.key, d.base)).eq(s));
        // All of it moved over from the settled tree: that tree again, so
        // settled as it was, with no layout to check it by.
        // Or rebuilt in parts, none of which moved a box (a hover's
        // highlight), and with the outputs it read made again: what the
        // size pass would find, it found last time.
        if kept && !t.reshaped && (!t.built || (!t.boxes_moved && t.outputs_settled())) {
            settled = Some(Seed {
                sizes: std::mem::take(&mut t.sizes),
                mins: std::mem::take(&mut t.mins),
                transforms: std::mem::take(&mut t.transforms),
                outputs: t.read_outputs(),
                bases: std::mem::take(&mut t.prev_bases),
                shape: t.insts.iter().map(|d| (d.key, d.base)).collect(),
                tree: None,
                opts: opts.clone(),
                laid: last_laid.take(),
            });
        }
        // Bindings read widget sizes (half a bar's height centres a label),
        // but an unbound size is only known once the tree is built and laid
        // out. Build again with the first layout's sizes. What bindings hide
        // is not expanded, so the shape can change between builds; each
        // instance finds its previous sizes by key. Zero sizes are not fed
        // back (see `add`).
        if opts.bindings && settled.is_none() {
            for _ in 0..SIZE_PASSES {
                // From the build before's layout (the last layout's, at
                // first): most of the tree is the same.
                let laid = evaluate_since(
                    &t.nodes,
                    0,
                    reference.unwrap_or([1920., 1080.]),
                    &|_| true,
                    &t.pass_of,
                    last_laid.as_ref(),
                    &|_| false,
                );
                let placed = &laid.placed;
                let mut styles: HashMap<Guid, Option<f32>> = HashMap::new();
                // Only sizes some build reads: the next build depends on
                // nothing else, so neither does settling. A node that starts
                // being read gets its size the round after.
                let mut read = vec![0u64; t.nodes.len().div_ceil(64)];
                let mut read_at = vec![0u64; t.nodes.len().div_ceil(64)];
                for r in &t.reads {
                    let set = match r {
                        Read::Size(..) => &mut read,
                        Read::At(..) => &mut read_at,
                    };
                    let j = r.node();
                    set[j / 64] |= 1 << (j % 64);
                }
                let is_read = |j: usize| read[j / 64] >> (j % 64) & 1 == 1;
                let is_read_at = |j: usize| read_at[j / 64] >> (j % 64) & 1 == 1;
                let transforms: Vec<[f32; 6]> = (0..t.nodes.len())
                    .map(|k| match placed.get(k) {
                        Some(p) if is_read_at(k) => p.transform,
                        _ => [f32::NAN; 6],
                    })
                    .collect();
                let sizes = t
                    .nodes
                    .iter()
                    .enumerate()
                    .map(|(k, n)| {
                        if !is_read(k) {
                            return [f32::NAN; 2];
                        }
                        let s = n.geometry.scale;
                        // A container has no size of its own: its box is the
                        // union of its visible children (the governors bar
                        // centres its labels on half its band's height).
                        // Labels store a 0×0 DisplayedSize when their text
                        // arrives at runtime.
                        let own = n
                            .intrinsic
                            .filter(|i| i[0] > 0. || i[1] > 0.)
                            .or_else(|| {
                                t.label_size(k, asset, &mut *load, &mut *measure, &mut styles)
                            })
                            .or_else(|| {
                                let p = placed.get(k)?;
                                Some([
                                    p.local.max[0] - p.local.min[0],
                                    p.local.max[1] - p.local.min[1],
                                ])
                            });
                        own.map_or([f32::NAN; 2], |i| [i[0] * s[0].abs(), i[1] * s[1].abs()])
                    })
                    .collect::<Vec<[f32; 2]>>();
                let mins_at = |placed: &[Placed], j: usize| placed.get(j).map(|p| p.local.min);
                let mins = placed.iter().map(|p| p.local.min).collect();
                // Settled: this tree was built from these very sizes.
                let same = |a: &[[f32; 2]], b: &[[f32; 2]]| {
                    a.len() == b.len()
                        && (0..a.len()).filter(|&j| is_read(j)).all(|j| {
                            let (x, y) = (a[j], b[j]);
                            (0..2).all(|i| {
                                x[i].to_bits() == y[i].to_bits() || (x[i] - y[i]).abs() < 0.01
                            })
                        })
                };
                let same_at = |a: &[[f32; 6]], b: &[[f32; 6]]| {
                    a.len() == b.len()
                        && (0..a.len()).filter(|&j| is_read_at(j)).all(|j| {
                            (0..6).all(|i| {
                                let (x, y) = (a[j][i], b[j][i]);
                                x.to_bits() == y.to_bits() || (x - y).abs() < 0.01
                            })
                        })
                };
                let outputs_settled = t.outputs_settled();
                if !t.reshaped
                    && same(&sizes, &t.sizes)
                    && same_at(&transforms, &t.transforms)
                    && outputs_settled
                {
                    passes.push(std::mem::take(&mut t.evals));
                    let shape: Vec<(u64, usize)> =
                        t.insts.iter().map(|d| (d.key, d.base)).collect();
                    let mut seed = Seed {
                        sizes: std::mem::take(&mut t.sizes),
                        mins: std::mem::take(&mut t.mins),
                        transforms: std::mem::take(&mut t.transforms),
                        outputs: t.read_outputs(),
                        bases: std::mem::take(&mut t.prev_bases),
                        shape,
                        tree: None,
                        opts: opts.clone(),
                        laid: None,
                    };
                    // Collapsing renumbers the nodes the layout is per.
                    if !t.collapse(placed, &mut seed) {
                        seed.laid = Some(laid);
                    }
                    settled = Some(seed);
                    break;
                }
                // A subtree builds the same again when none of the sizes it
                // read changed and nothing in it whose outputs its parent
                // reads was evaluated anew; its root's own feed is checked
                // when it is reached (see `add`).
                let reusable = {
                    let mut dirty = vec![0usize; t.reads.len() + 1];
                    for (r, read) in t.reads.iter().enumerate() {
                        let bits = |v: Option<[f32; 2]>| v.map(|v| v.map(f32::to_bits));
                        let moved = match *read {
                            Read::Size(j, size, min) => {
                                bits(size) != bits(sizes.get(j).copied())
                                    || min
                                        .is_some_and(|m| bits(Some(m)) != bits(mins_at(placed, j)))
                            }
                            Read::At(j, at) => {
                                at.map(|a| a.map(f32::to_bits))
                                    != transforms.get(j).map(|a| a.map(f32::to_bits))
                            }
                        };
                        dirty[r + 1] = dirty[r] + usize::from(moved);
                    }
                    let mut bad = vec![0usize; t.insts.len() + 1];
                    for (r, d) in t.insts.iter().enumerate() {
                        bad[r + 1] = bad[r] + usize::from(d.fresh && d.outputs_read);
                    }
                    bits(t.insts.iter().enumerate().map(|(r, d)| {
                        dirty[d.reads.1] == dirty[d.reads.0] && bad[d.last] == bad[r + 1]
                    }))
                };
                let mut again = Tree {
                    bindings: opts.bindings,
                    play: opts.play,
                    preview: opts.preview.clone(),
                    shown: opts.shown,
                    sims: opts.sims.clone(),
                    sample: opts.sample,
                    sizes,
                    mins,
                    transforms,
                    cache: std::mem::take(&mut t.cache),
                    root,
                    prev_bases: t
                        .insts
                        .iter()
                        .enumerate()
                        .map(|(r, d)| (d.key, (r, d.base)))
                        .collect(),
                    reusable,
                    prev_evals: {
                        passes.push(t.evals.clone());
                        std::mem::take(&mut t.evals)
                    },
                    pass: t.pass + 1,
                    layout_evals: std::mem::take(&mut t.layout_evals),
                    same_evals: std::mem::take(&mut t.same_evals),
                    sampled_data: std::mem::take(&mut t.sampled_data),
                    stale: std::mem::take(&mut t.stale),
                    record_reads: t.record_reads,
                    same_reads: std::mem::take(&mut t.same_reads),
                    reads_by: std::mem::take(&mut t.reads_by),
                    ..Tree::default()
                };
                again.reserve_like(&t);
                // Moved-over parts leave `t` without them.
                let shape: Vec<(u64, usize)> = t.insts.iter().map(|d| (d.key, d.base)).collect();
                again.take_values(&mut t);
                again.prev = Some(Box::new(t));
                again.add(asset, 0, load, 0, Parent::default());
                again.prune_values();
                if let Some(root) = again.nodes.first_mut() {
                    root.size = reference;
                }
                let prev = again.prev.take().expect("the previous build");
                again.reshaped = !again.insts.iter().map(|d| (d.key, d.base)).eq(shape);
                drop_later(prev);
                t = again;
                last_laid = Some(laid);
            }
            t.sizes = Vec::new();
            t.mins = Vec::new();
        }
        if !t.evals.is_empty() {
            passes.push(std::mem::take(&mut t.evals));
        }
        // The last layout's evaluations are in `passes` now.
        drop_later(std::mem::take(&mut t.layout_evals));
        *cache = std::mem::take(&mut t.cache);
        if cache.layouts.len() >= CACHED_LAYOUTS {
            drop_later(cache.layouts.remove(0));
        }
        cache.layouts.push((root, passes, settled));
        t
    }

    /// A label's size from its text and TextStyle size, for bindings that
    /// read it (the minimap's lode badge is its label's longer side + 20).
    /// Rough: 0.52 em per glyph, 1.25 em per line, as the editor draws
    /// labels it has no font for; the editor measures with the font later.
    /// Labels whose text is only a LocalizationId get none; labels with no
    /// text at all are zero wide.
    fn label_size(
        &self,
        node: usize,
        root: &Asset,
        load: &mut dyn FnMut(Guid) -> Option<Arc<Asset>>,
        measure: Measure,
        styles: &mut HashMap<Guid, Option<f32>>,
    ) -> Option<[f32; 2]> {
        let (file, w) = *self.source.get(node)?;
        let a: &Asset = if file == 0 {
            root
        } else {
            self.templates.get(file - 1)?
        };
        let widget = self.cache.templates.get(&a.guid()?)?.parsed.0.get(w)?;
        let label = a.label(widget)?;
        let text = self.nodes[node]
            .text
            .clone()
            .into_iter()
            .chain([label.displayed_text.clone(), label.text.clone()])
            .find(|t| !t.is_empty());
        if text.is_none() && !label.localization_id.is_empty() {
            return None;
        }
        let text = text.unwrap_or_default();
        let style = label.style?.id;
        let size = (*styles.entry(style).or_insert_with(|| {
            load(style)
                .and_then(|s| super::text_style(&s))
                .map(|s| s.size)
        }))?;
        let b = &self.nodes[node].bound.text_plane;
        let plane = [0, 1].map(|a| b[a].unwrap_or(label.text_plane[a]));
        // Without the font: 0.52 em per glyph.
        let mut width =
            |t: &str| measure(style, t).unwrap_or(t.chars().count() as f32 * size * 0.52);
        Some(label_box(&label, &text, size * LINE_EM, plane, &mut width).1)
    }

    /// Per node: the node of the previous layout of the same root (the tree
    /// handed back by [`Cache::keep`]) that it is the same as, children
    /// apart, or [`NOT_KEPT`]. What the editor made of that node holds for
    /// this one wherever what is around it is the same too.
    pub fn previous(&self) -> &[u32] {
        &self.prev_of
    }

    /// The keys of every instance of template `g` in this tree: what a
    /// layout after an edit of `g` rebuilds ([`Options::stale`]).
    pub fn instances_of(&self, g: Guid) -> Vec<u64> {
        let mut out: Vec<u64> = self
            .insts
            .iter()
            .filter(|d| d.file != 0 && self.templates[d.file - 1].guid() == Some(g))
            .map(|d| d.key)
            .collect();
        out.sort_unstable();
        out.dedup();
        out
    }

    /// Every file instance's key by the node it is keyed by in
    /// [`Tree::traces`]: its container's, 0 for the tree's own asset.
    pub fn instance_keys(&self) -> FastMap<usize, u64> {
        self.insts
            .iter()
            .map(|d| (d.container.unwrap_or(0), d.key))
            .collect()
    }

    /// A file instance's template property values, by instance key: what
    /// its container passed in, and what its own bindings wrote, which
    /// win.
    pub fn template_values(&self, key: u64) -> (Option<Values>, Option<Values>) {
        (
            self.inputs_in.get(&key).cloned(),
            self.outputs.get(&key).cloned(),
        )
    }

    /// Whether the instance at `node` or one of its near ancestors is named
    /// like a list of events. Lists share generic names
    /// (`InteractionElementArray`); the container around says what for.
    fn under_event_list(&self, node: Option<usize>) -> bool {
        // Not "notification": the archive's notification buttons are
        // always there. Untracked quests only list when expanded.
        const EVENTS: [&str; 7] = [
            "buff",
            "incidence",
            "incident",
            "warinfo",
            "alert",
            "warning",
            "untracked",
        ];
        self.ancestor_names(node, 6)
            .iter()
            .any(|n| EVENTS.iter().any(|e| n.to_ascii_lowercase().contains(e)))
    }

    /// The name of the widget at `node`, to tell what a list is for.
    fn name_of(&self, node: usize) -> Option<&str> {
        let (file, w) = *self.source.get(node)?;
        let g = match file {
            0 => self.root?,
            f => self.templates.get(f - 1)?.guid()?,
        };
        self.cache
            .templates
            .get(&g)?
            .parsed
            .0
            .get(w)?
            .name
            .as_deref()
    }

    /// Names of the widget at `node` and its ancestors, nearest first, up to
    /// `depth`. Inside a template being expanded, its root has no parent
    /// yet: the walk steps to the instance that expands it.
    fn ancestor_names(&self, node: Option<usize>, depth: usize) -> Vec<String> {
        let mut out = Vec::new();
        let mut at = node;
        for _ in 0..depth {
            let Some(i) = at else { break };
            if let Some(n) = self.name_of(i) {
                out.push(n.to_owned());
            }
            at = self
                .parent
                .get(i)
                .copied()
                .flatten()
                .or_else(|| self.instance.get(i).copied().flatten());
        }
        out
    }

    // Appends `asset`'s widgets as file `file`; returns its root's index.
    fn add(
        &mut self,
        asset: &Asset,
        file: usize,
        load: &mut dyn FnMut(Guid) -> Option<Arc<Asset>>,
        depth: usize,
        from: Parent,
    ) -> usize {
        let Parent {
            instance,
            inputs,
            writes,
            slice,
            index,
            item,
            context,
            fed,
            bound,
            wheel_groups,
            components: instance_components,
            hashes: (mut inputs_hash, mut context_hash),
            lists,
            started,
            key: ikey,
            sized: from_sized,
            hidden: from_hidden,
            reuse: from_reuse,
        } = from;
        // The container instance stands in for the template root (see
        // `binding::Feed::root_geometry`); a collapsed subtree depends on it.
        let root_geometry = instance.map(|i| self.nodes[i].geometry.clone());
        let incoming = if from_hidden {
            incoming(&from)
                ^ root_geometry
                    .as_ref()
                    .map_or(0, super::binding::geometry_hash)
                | 1
        } else {
            0
        };
        let no_context = HashMap::new();
        let context = context.unwrap_or(&no_context);
        // Items sample apart (see `Parent::item`): what they evaluate to is
        // not shared between them.
        if self.sample.is_some()
            && let Some(item) = item
        {
            use std::hash::{Hash, Hasher};
            let mut h = hasher();
            (context_hash, item).hash(&mut h);
            context_hash = h.finish();
        }
        let tpl_info = self.cache.template(asset, load);
        let parsed = &tpl_info.parsed;
        let widgets = &parsed.0;
        let base = self.nodes.len();
        // This instance's base in the previous build, which the sizes,
        // outputs and evaluations it reads are indexed by.
        let old_rec = self.prev_bases.get(&ikey).map(|o| o.0);
        let old = self.prev_bases.get(&ikey).map(|o| o.1);
        let rec = self.insts.len();
        self.insts.push(Inst {
            key: ikey,
            base,
            file,
            container: instance,
            fresh: true,
            sized: from_sized,
            reads: (self.reads.len(), 0),
            incoming,
            ..Inst::default()
        });
        if from_reuse && let Some(r) = old_rec {
            self.reuse(r, rec);
            return base;
        }
        // A collapsed hidden subtree handed what it was built from is as
        // it was: it reads nothing from outside but that.
        if let Some(r) = old_rec
            && incoming != 0
            && on(&self.reusable, r)
            && self
                .prev
                .as_ref()
                .is_some_and(|p| p.insts[r].collapsed.is_some() && p.insts[r].incoming == incoming)
        {
            self.reuse(r, rec);
            return base;
        }
        let key = asset.guid();
        // What its container passes in has sample values filled in already
        // (see `ChildFeed`); preview values (set by hand) win over everything.
        // Recomputed below where the inputs change here.
        let mut rehash = inputs.is_none();
        let mut shared_inputs: Values = match inputs {
            Some(v) => v.clone(),
            None => {
                context_hash = self::context_hash(context);
                Arc::new(tpl_info.with_samples(asset, self.sample, HashMap::new(), None))
            }
        };
        // A list stamps sample items (below); its bindings size them from
        // the native TotalItemCount, which the game takes from DataSource.
        let list = tpl_info.list.filter(|_| self.sample.is_some());
        // Lists of things that happen (buffs, incidents, wars, alerts) are
        // empty in an idle game: no items, the prototype hidden.
        let idle = list.is_some() && self.under_event_list(instance);
        let list = list.filter(|_| !idle);
        let radial = list.is_some() && tpl_info.radial;
        // Scene content may describe a list item by item (the minimap
        // ring's buttons, lodes and fertilities): the sampler answers
        // `<DataSource>#len`. Only such a list's items sample apart (see
        // `Parent::item`); the rest share what they evaluate to, as the
        // radial menu's thousands of items must.
        let described: Option<(&str, usize)> =
            list.and(context.get(&LIST_DATA_SOURCE)).and_then(|src| {
                match self.sample?(&format!("{src}#len"), 1, &DefaultValue::None)? {
                    DefaultValue::S32(n) if n > 0 => Some((src.as_str(), n as usize)),
                    _ => None,
                }
            });
        let sample_items = described.map_or_else(|| sample_count(radial, lists), |(_, n)| n);
        if list.is_some() || idle {
            let count = if idle { 0 } else { sample_items };
            Arc::make_mut(&mut shared_inputs)
                .insert(LIST_TOTAL_ITEM_COUNT, DefaultValue::U32(count as u32));
            rehash = true;
        }
        if let Some(p) = key.and_then(|k| self.preview.get(&k)) {
            Arc::make_mut(&mut shared_inputs).extend(p.iter().map(|(k, v)| (*k, v.clone())));
            rehash = true;
        }
        if rehash {
            inputs_hash = values_hash(&shared_inputs);
        }
        // Kept per instance (`inputs_in`) as it is now.
        let inputs = &*shared_inputs;
        // A template input its parent neither feeds nor binds is
        // filled by the game only on the scene root (its game
        // context) and on list items (the list hands each its entry).
        // Elsewhere it stays empty, and so does what it feeds: the
        // quest tracker header's IconText, which the patch of
        // 2026-11-05 added unbound beside the old title, draws
        // nothing in game.
        let unfed = |u: u32| {
            file != 0
                && index.is_none()
                && fed.binary_search(&u).is_err()
                && bound.binary_search(&u).is_err()
                && !inputs.contains_key(&u)
                && asset
                    .properties()
                    .iter()
                    .position(|p| p.uid == u)
                    .is_some_and(|i| !tpl_info.table.writes_property(i))
        };
        let clips = &tpl_info.clips;
        let mut animated: Vec<(usize, DefaultValue)> = Vec::new();
        let table = &tpl_info.table;
        // The opening clips' end state: clips by name, those its own
        // bindings Start as the screen opens, and those its parent Starts.
        // What a parent Starts varies per instance; the rest is the same
        // for every instance of the template.
        // State-started clips fire by the instance's inputs (and, below, by
        // what its bindings compute from them).
        let per_instance = !table.state_clips.is_empty();
        let fired = if per_instance {
            fired_state_clips(asset, &table.state_clips, inputs, None)
        } else {
            Vec::new()
        };
        let settled: Arc<Settled> = if !self.shown {
            Arc::default()
        } else if started.is_empty() && !per_instance {
            tpl_info.settled.clone()
        } else {
            Arc::new(opening_end(
                clips,
                &table.opening_clips,
                &table.state_clips,
                &fired,
                started,
            ))
        };
        // Simulated interaction states (hover, press) on one instance.
        for sim in &self.sims {
            let ours = match sim.file {
                None => file == 0,
                Some(g) => file != 0 && asset.guid() == Some(g) && Some(ikey) == sim.instance,
            };
            if ours && let Some(c) = clips.get(sim.clip) {
                animated.extend(c.sample(c.duration()));
            }
        }
        // The clip being played writes last, so it wins.
        if let Some(p) = self.play.filter(|p| match p.file {
            None => file == 0,
            Some(g) => file != 0 && asset.guid() == Some(g),
        }) && let Some(c) = clips.get(p.clip)
        {
            animated.extend(c.sample(p.time));
        }
        // What evaluation produced, shared with the next size pass.
        let mut shared: Option<Arc<Expansion>> = None;
        // Evaluated from the same feed as in the previous build.
        let mut own_hit = false;
        if self.bindings
            || !animated.is_empty()
            || !settled.is_empty()
            || !inputs.is_empty()
            || !writes.is_empty()
        {
            let sampling = self.sample.is_some();
            let child_lists = lists.saturating_add(u8::from(list.is_some()));
            // Both lookups load assets; they take turns with the loader.
            let loader = std::cell::RefCell::new(&mut *load);
            let mut defaults = |template: Guid, uid: u32| {
                let mut load = loader.borrow_mut();
                let a = load(template)?;
                // A list instance shows its sample items; its parent sizes
                // it by their count.
                if uid == LIST_TOTAL_ITEM_COUNT && sampling && is_list(&a, &mut **load) {
                    let radial = derives_from(&a, &mut **load, RADIAL_WIDGET_TYPE);
                    let n = sample_count(radial, child_lists);
                    return Some(DefaultValue::U32(n as u32));
                }
                let p = a.properties().iter().find(|p| p.uid == uid)?;
                Some(a.default_at(p.default))
            };
            let mut enum_value = |enum_asset: Guid, rule: u32, entry: u32| {
                let a = (loader.borrow_mut())(enum_asset)?;
                a.enum_rule(rule, entry)
            };
            let ev = if self.bindings {
                let sample = self.sample;
                // The sampler sees the whole path by name (template property,
                // then game-object fields: `IconTextData.TextData.Text`), so
                // a generic `Text` field can still be told apart.
                // Data a list hands its items (no binding to name it): the
                // widgets around say what it is (`Objectives_Container`).
                // Built only when a game-data path is sampled.
                let around = std::cell::OnceCell::new();
                let around = || {
                    around.get_or_init(|| {
                        let mut a = self.ancestor_names(instance, 16);
                        a.reverse();
                        a.join(".")
                    })
                };
                // With `record_reads`: every path this evaluation asked for.
                let read_names: std::cell::RefCell<Vec<String>> = Default::default();
                let record = self.record_reads;
                let mut game_data = |path: &[(u32, u32)]| {
                    if sample.is_some() && path.last()?.1 == UI_SCALE {
                        return Some(DefaultValue::F32(1f32.to_bits()));
                    }
                    let (_, _, kind) = super::tables::data_field(path.last()?.1)?;
                    let first = path.first()?.1;
                    let around_named = !context.contains_key(&first)
                        && asset.properties().iter().any(|p| p.uid == first);
                    // An empty input reads as zero in game: false, 0, no
                    // text (the close button's lock stays off).
                    if around_named && unfed(first) {
                        return match kind {
                            0 => Some(DefaultValue::Bool(0)),
                            1 => Some(DefaultValue::S32(0)),
                            2 => Some(DefaultValue::U32(0)),
                            3 => Some(DefaultValue::F32(0)),
                            4 => Some(DefaultValue::Str(String::new())),
                            _ => None,
                        };
                    }
                    // What the name below is built from (the context only
                    // names steps of the path).
                    let key = {
                        use std::hash::{Hash, Hasher};
                        let mut h = hasher();
                        std::ptr::from_ref(asset).hash(&mut h);
                        path.hash(&mut h);
                        for (_, id) in path {
                            context.get(id).hash(&mut h);
                        }
                        if around_named {
                            around().hash(&mut h);
                        }
                        item.hash(&mut h);
                        h.finish()
                    };
                    let name = || {
                        let mut name = path_name(asset, path, context);
                        // The open file's own data has no widgets around it
                        // (its root is unnamed): no leading dot then.
                        if around_named && !around().is_empty() {
                            name = format!("{}.{name}", around());
                        }
                        if let Some((list, n)) = item {
                            name = format!("{list}#{n}:{name}");
                        }
                        name
                    };
                    if record {
                        read_names.borrow_mut().push(name());
                    }
                    if let Some(v) = self.sampled_data.borrow().get(&key) {
                        return v.clone();
                    }
                    let name = name();
                    let v = sample?(&name, kind, &DefaultValue::None);
                    self.sampled_data.borrow_mut().insert(key, v.clone());
                    v
                };
                let mut root_components: Vec<(Guid, u32, DefaultValue)> = slice
                    .map(|(a, size, p)| {
                        let f = |x: f32| DefaultValue::F32(x.to_bits());
                        vec![
                            (Guid::from_const(RADIAL_ITEM), RADIAL_ITEM_ROTATION, f(a)),
                            (
                                Guid::from_const(RADIAL_ITEM),
                                RADIAL_ITEM_ANGLE_SIZE,
                                f(size),
                            ),
                            (
                                Guid::from_const(RADIAL_ITEM),
                                RADIAL_ITEM_POSITION,
                                DefaultValue::Vec(vec![p[0].to_bits(), p[1].to_bits(), 0]),
                            ),
                        ]
                    })
                    .unwrap_or_default();
                // The wheel writes one step into the list's own sector info
                // too: the minimap ring starts one step before 138°
                // (`StartAngle = 138 - SecorAngle`), which puts its speed
                // buttons around the bottom as in game.
                if radial {
                    let groups: Vec<WheelGroup> = wheel_groups
                        .iter()
                        .map(|&uid| WheelGroup::read(uid, instance_components))
                        .collect();
                    let total = floats(inputs.get(&RADIAL_TOTAL_ANGLE))
                        .and_then(|v| v.first().copied())
                        .filter(|t| *t > 0.)
                        .unwrap_or(360.);
                    root_components.push((
                        Guid::from_const(RADIAL_ITEM),
                        RADIAL_ITEM_ANGLE_SIZE,
                        DefaultValue::F32(wheel_step(total, &groups, sample_items).to_bits()),
                    ));
                }
                if let Some(n) = index {
                    root_components.push((
                        Guid::from_const(INDEX_EXPOSER),
                        INDEX_DATA_SOURCE,
                        DefaultValue::U32(n),
                    ));
                }
                // Evaluation reads outputs and sizes of these widgets only.
                let read = self.prev_outputs.as_ref().unwrap_or(&self.outputs);
                let mut outputs: Vec<Option<&HashMap<u32, DefaultValue>>> =
                    vec![None; widgets.len()];
                for &k in table.output_sources.iter().filter(|&&k| k < widgets.len()) {
                    outputs[k] = read.get(&instance_key(ikey, k)).map(|v| &**v);
                }
                let mut sizes: Vec<Option<[f32; 2]>> = vec![None; widgets.len()];
                if let Some(old) = old {
                    for &k in table.box_readers.iter().filter(|&&k| k < widgets.len()) {
                        // Zero at Scale 0: stored for a tween to grow at
                        // runtime, not a size to build on. Zero at a real
                        // scale is an empty container (a pin slot's loader
                        // with no object), which bindings test for.
                        let sc = parsed.1[k].geometry.scale;
                        let shrunk = sc[0] == 0. || sc[1] == 0.;
                        sizes[k] = self
                            .sizes
                            .get(old + k)
                            .copied()
                            .filter(|s| (s[0] > 0. && s[1] > 0.) || (!shrunk && *s == [0.; 2]));
                    }
                }
                // Where the build before laid out the widgets a Positioning
                // component converts by.
                let mut transforms: Vec<Option<[f32; 6]>> = vec![None; widgets.len()];
                if let Some(old) = old {
                    for &k in table.positioned.iter().filter(|&&k| k < widgets.len()) {
                        transforms[k] = self
                            .transforms
                            .get(old + k)
                            .copied()
                            .filter(|t| !t[0].is_nan());
                    }
                }
                let feed = super::binding::Feed {
                    inputs,
                    sizes: &sizes,
                    transforms: &transforms,
                    root_geometry: root_geometry.as_ref(),
                    animated: &animated,
                    settled: &settled[..],
                    inherited: writes,
                    outputs: &outputs,
                    root_components: &root_components,
                    instance_components,
                    prepared: Some(table),
                    sampling: self.sample.is_some(),
                    inputs_hash: Some(inputs_hash),
                    settled_hash: (started.is_empty() && self.shown && !per_instance)
                        .then_some(tpl_info.settled_hash),
                };
                // The names game data is sampled by come from the parent.
                let fingerprint = {
                    use std::hash::{Hash, Hasher};
                    let mut h = hasher();
                    feed.fingerprint().hash(&mut h);
                    key.hash(&mut h);
                    context_hash.hash(&mut h);
                    h.finish()
                };
                // Game data a stale instance reads answers differently now:
                // what the last build evaluated does not hold for it. This
                // build's own earlier passes do.
                let stale = self.stale.binary_search(&ikey).is_ok();
                let stale_now = stale && self.pass == 0;
                let hit = !stale_now
                    && self
                        .prev_evals
                        .get(&ikey)
                        .is_some_and(|(f, _)| *f == fingerprint);
                own_hit = hit;
                // The same feed and nothing below changed: the subtree is
                // what the previous build made of it.
                if hit
                    && let Some(r) = old_rec
                    && on(&self.reusable, r)
                    && self
                        .prev
                        .as_ref()
                        .is_some_and(|p| p.insts[r].sized == from_sized)
                {
                    self.reuse(r, rec);
                    return base;
                }
                let own_reads: Vec<_> = table
                    .box_readers
                    .iter()
                    .filter(|&&w| w < widgets.len())
                    .map(|&w| {
                        Read::Size(
                            base + w,
                            old.and_then(|o| self.sizes.get(o + w)).copied(),
                            None,
                        )
                    })
                    .chain(
                        table
                            .positioned
                            .iter()
                            .filter(|&&w| w < widgets.len())
                            .map(|&w| Read::At(base + w, transforms[w])),
                    )
                    .collect();
                // A later size pass re-evaluates only instances whose feed
                // changed: by the fifth, a few of thousands.
                // Or this pass of the previous layout of the same root; the
                // root's own file is the one the editor changes.
                let last_layout = (file != 0 && !stale)
                    .then(|| self.layout_evals.get(self.pass)?.get(&ikey))
                    .flatten();
                // Where this evaluation is shared, once it is made.
                let mut new_same = None;
                let mut shared_from = None;
                let ev = match self
                    .prev_evals
                    .get(&ikey)
                    .filter(|(f, _)| *f == fingerprint && !stale_now)
                    .or(last_layout)
                {
                    Some((f, exp)) if *f == fingerprint => exp.clone(),
                    _ => {
                        use std::hash::{Hash, Hasher};
                        // Instances of a template fed alike evaluate alike
                        // (most of a screen's list items and repeated
                        // buttons): the same asset, feed and game-data
                        // names. Those come from the context, which the
                        // fingerprint holds, and for a template property
                        // the context does not name, from the widgets
                        // around. The root's file is the edited one, and
                        // has one instance.
                        let around_read = table.paths.iter().any(|p| {
                            p.first().is_some_and(|&(_, f)| {
                                !context.contains_key(&f)
                                    && asset.properties().iter().any(|q| q.uid == f)
                            })
                        });
                        let same = (file != 0).then(|| {
                            let mut h = hasher();
                            fingerprint.hash(&mut h);
                            // Assets stay loaded for the whole layout.
                            std::ptr::from_ref(asset).hash(&mut h);
                            child_lists.hash(&mut h);
                            if around_read {
                                around().hash(&mut h);
                            }
                            h.finish()
                        });
                        match same
                            .filter(|_| !stale_now)
                            .and_then(|k| Some((k, self.same_evals.get(&k)?)))
                        {
                            Some((k, exp)) => {
                                shared_from = Some(k);
                                exp.clone()
                            }
                            None => {
                                let mut ev = super::binding::evaluate(
                                    asset,
                                    widgets,
                                    &feed,
                                    &mut defaults,
                                    &mut game_data,
                                    &mut enum_value,
                                );
                                // A trigger the bindings compute (a flag
                                // from PanelType) fires its clip: evaluate
                                // again with that clip's end state.
                                if per_instance && self.shown {
                                    let now = fired_state_clips(
                                        asset,
                                        &table.state_clips,
                                        inputs,
                                        Some(&ev.template),
                                    );
                                    if now.iter().any(|c| !fired.contains(c)) {
                                        let settled = opening_end(
                                            clips,
                                            &table.opening_clips,
                                            &table.state_clips,
                                            &now,
                                            started,
                                        );
                                        let again = super::binding::Feed {
                                            settled: &settled[..],
                                            settled_hash: None,
                                            ..feed
                                        };
                                        ev = super::binding::evaluate(
                                            asset,
                                            widgets,
                                            &again,
                                            &mut defaults,
                                            &mut game_data,
                                            &mut enum_value,
                                        );
                                    }
                                }
                                new_same = same;
                                Arc::new(Expansion::new(
                                    ev,
                                    &tpl_info,
                                    asset,
                                    inputs,
                                    true,
                                    depth > 0,
                                ))
                            }
                        }
                    }
                };
                if self.record_reads {
                    let mut names = read_names.take();
                    if let Some(k) = shared_from {
                        names = self.same_reads.get(&k).cloned().unwrap_or_default();
                    }
                    if let Some(k) = new_same {
                        self.same_reads.insert(k, names.clone());
                    }
                    let mut by = self.reads_by.borrow_mut();
                    for n in names {
                        let keys = by.entry(n).or_default();
                        if keys.last() != Some(&ikey) {
                            keys.push(ikey);
                        }
                    }
                }
                if let Some(k) = new_same {
                    self.same_evals.insert(k, ev.clone());
                }
                self.evals.insert(ikey, (fingerprint, ev.clone()));
                self.insts[rec].fresh = !hit;
                self.built = true;
                self.reads.extend(own_reads);
                ev
            } else {
                // Without bindings, clip end states are plain writes.
                let all: Vec<_> = settled.iter().chain(&animated).cloned().collect();
                let ev = super::binding::evaluate_writes(asset, widgets, &all);
                Arc::new(Expansion::new(
                    ev,
                    &tpl_info,
                    asset,
                    inputs,
                    false,
                    depth > 0,
                ))
            };
            let mut out = ev.ev.template.clone();
            // What its parent reads as the list's item count.
            if let Some(n) = inputs.get(&LIST_TOTAL_ITEM_COUNT)
                && (list.is_some() || idle)
            {
                Arc::make_mut(&mut out).insert(LIST_TOTAL_ITEM_COUNT, n.clone());
            }
            let was = self.outputs.insert(ikey, out);
            self.overwritten.entry(ikey).or_insert(was);
            self.traces.insert(ikey, ev.ev.trace.clone());
            self.inputs_in.insert(ikey, shared_inputs.clone());
            shared = Some(ev);
        }
        let exp = shared.unwrap_or_else(|| {
            let ev = super::binding::Evaluated::default();
            Arc::new(Expansion::new(
                ev,
                &tpl_info,
                asset,
                inputs,
                false,
                depth > 0,
            ))
        });
        let ev_view = &exp.ev;
        let nested_writes = &ev_view.nested;
        self.prev_of.resize(base + exp.nodes.len(), NOT_KEPT);
        self.pass_of.resize(base + exp.nodes.len(), NOT_KEPT);
        self.nodes.extend(exp.nodes.iter().map(|n| {
            let mut n = n.clone();
            for c in &mut n.children {
                *c += base;
            }
            n
        }));
        self.source.extend((0..widgets.len()).map(|k| (file, k)));
        self.instance.extend((0..widgets.len()).map(|_| instance));
        if idle && let [only] = widgets[0].children[..] {
            self.nodes[base + only].geometry.visible = false;
        }
        self.parent
            .extend(widgets.iter().map(|w| w.parent.map(|p| p + base)));
        // (widget, node) pairs to finish: every widget, then the sample
        // items stamped from a list's prototype.
        let mut todo: Vec<(usize, usize)> = (0..widgets.len()).map(|k| (k, base + k)).collect();
        // A radial list's items: (angle, angle size, position) per stamped
        // item, the prototype first.
        let mut slices: Vec<Option<(f32, f32, [f32; 2])>> = Vec::new();
        if let Some(proto) = list {
            for item in 1..sample_items {
                let i = self.nodes.len();
                let mut copy = self.nodes[base + proto].clone();
                copy.children.clear();
                copy.item = item as u32;
                self.nodes.push(copy);
                self.prev_of.push(NOT_KEPT);
                self.pass_of.push(NOT_KEPT);
                self.source.push((file, proto));
                self.instance.push(instance);
                self.parent.push(Some(base));
                self.nodes[base].children.push(i);
                todo.push((proto, i));
            }
            // The list's settings: what the instance passes, else what the
            // template stores on its root.
            let own: HashMap<u32, DefaultValue> = asset
                .property_values(&widgets[0])
                .into_iter()
                .filter_map(|(path, v)| match path[..] {
                    [uid] => Some((uid, v)),
                    _ => None,
                })
                .collect();
            let get = |uid: u32| inputs.get(&uid).or_else(|| own.get(&uid));
            let f = |uid: u32| floats(get(uid)).and_then(|v| v.first().copied());
            if radial {
                // WidgetType defaults: 0, 360, 100.
                let start = f(RADIAL_START_ANGLE).unwrap_or(0.);
                let total = f(RADIAL_TOTAL_ANGLE).filter(|t| *t > 0.).unwrap_or(360.);
                let radius = f(RADIAL_RADIUS).unwrap_or(100.);
                let groups: Vec<WheelGroup> = wheel_groups
                    .iter()
                    .map(|&uid| WheelGroup::read(uid, instance_components))
                    .collect();
                let windowed = match get(LIST_VISIBLE_ITEMS) {
                    Some(DefaultValue::S32(v)) => *v != 0,
                    Some(DefaultValue::U32(v)) => *v != 0,
                    _ => false,
                };
                slices = wheel_slices(start, total, radius, &groups, sample_items, windowed);
            }
            let flag = |uid| matches!(get(uid), Some(DefaultValue::Bool(b)) if *b != 0);
            let count = |uid| match get(uid) {
                Some(DefaultValue::U32(n)) => *n as usize,
                Some(DefaultValue::S32(n)) => (*n).max(0) as usize,
                _ => 2,
            };
            // Radial lists place their items through slices instead.
            if radial {
            } else if tpl_info.stacks {
                self.nodes[base].components.push(LayoutComponent::Stack {
                    horizontal: flag(LIST_IS_HORIZONTAL),
                    align: Align::Begin,
                    // The stack visits children last to first; the copies are
                    // alike, so order does not matter.
                    invert: false,
                    spacing: f(LIST_SPACING).unwrap_or(0.),
                    margin: [0.; 4],
                });
            } else {
                self.nodes[base].components.push(LayoutComponent::Grid {
                    across: flag(GRID_IS_HORIZONTAL),
                    columns: count(GRID_COLUMNS),
                    rows: count(GRID_ROWS),
                    spacing: [
                        f(GRID_HORIZONTAL_SPACING).unwrap_or(0.),
                        f(GRID_VERTICAL_SPACING).unwrap_or(0.),
                    ],
                });
            }
        }
        // The list prototype's feed varies per instance (whether the list
        // is idle), so it is worked out here, once for all its copies.
        let mut list_feed: Option<ChildFeed> = None;
        let mut slice_at = slices.iter();
        // The prototype is visited first, then its copies in order.
        let mut item_at = 0u32;
        let media = &tpl_info.media;
        let block_end = self.nodes.len();
        // A template's own bindings hiding its root hide the instance (see
        // below); the tree's root always draws.
        let root_hidden = file != 0 && self.nodes[base].bound.visible == Some(false) && !from_sized;
        for (k, i) in todo {
            if let Some(size) = media.get(k).copied().flatten() {
                self.nodes[i].intrinsic = Some(size);
            }
            if depth >= MAX_TEMPLATE_DEPTH {
                continue;
            }
            // A template that decodes to no widgets (one written with a class
            // version the schema does not know yet, as after a game patch)
            // draws nothing; the instance keeps its own box.
            let Some(tpl) = table.templates[k]
                .and_then(&mut *load)
                .filter(|t| !t.widgets().is_empty())
            else {
                continue;
            };
            let (slice, index) = if list == Some(k) {
                item_at += 1;
                (slice_at.next().copied().flatten(), Some(item_at - 1))
            } else {
                (None, None)
            };
            let item = match index {
                Some(n) => described.map(|(src, _)| (src, n)),
                None => item,
            };
            if slice.is_some() {
                // The list only writes each item's slice; the item's own
                // bindings turn and place its sector from it (the minimap
                // sector feeds Rotation and Position to its core). Turning
                // the item root as well counts the angle twice.
                self.placed.insert(i);
            }
            let child_info = self.cache.template(&tpl, load);
            let (size, fixed) = (child_info.reference, child_info.fixed);
            // Nothing under a hidden widget draws, and most expansions are
            // (35 of the radial menu's 200k nodes show): its template is
            // not instantiated or evaluated, unless this file's bindings
            // read what it would give, its outputs or the size of a box it
            // spans (a hidden widget keeps its size in game; the main
            // title's button backing reads its column's). It keeps its own
            // box, for finding it in the editor.
            // A box spans the widget while no hidden widget lies between
            // (a hidden one does not count toward its parent's box, but
            // keeps its own); the instance's box counts as read when its
            // parent reads it.
            let read = |w: usize| table.box_readers.binary_search(&w).is_ok();
            let mut hidden = false;
            let mut blocked = false;
            let mut sized = false;
            let mut at = i;
            loop {
                sized |= !blocked && (read(self.source[at].1) || (at == base && from_sized));
                if at == base {
                    hidden |= root_hidden;
                    break;
                }
                let shown = self.nodes[at].geometry.visible;
                hidden |= !shown;
                blocked |= !shown;
                match self.parent[at] {
                    Some(p) => at = p,
                    None => break,
                }
            }
            let outputs_read = table.output_sources.binary_search(&k).is_ok();
            if hidden && !sized && !outputs_read {
                let n = &mut self.nodes[i];
                n.size = size;
                if fixed {
                    n.intrinsic = size;
                }
                continue;
            }
            self.templates.push(tpl.clone());
            let child = self.insts.len();
            let child_key = instance_key(ikey, i - base);
            // What this instance hands the child follows from its own
            // feed and evaluation: unchanged, a child the previous build
            // left reusable is that again, with nothing worked out for it.
            let fast = own_hit
                && self.prev_bases.get(&child_key).is_some_and(|&(r, _)| {
                    on(&self.reusable, r)
                        && self
                            .prev
                            .as_ref()
                            .is_some_and(|p| p.insts[r].sized == sized)
                });
            let sub = if fast {
                let parent = Parent {
                    instance: Some(i),
                    key: child_key,
                    sized,
                    hidden,
                    reuse: true,
                    ..Parent::default()
                };
                self.add(&tpl, self.templates.len(), load, depth + 1, parent)
            } else {
                let make = || {
                    ChildFeed::new(ChildOf {
                        asset,
                        k,
                        table,
                        ev: &exp.ev,
                        tpl: &tpl,
                        child: &child_info,
                        context,
                        unfed: &unfed,
                        sample: self.sample,
                        list_root: list == Some(k),
                        shown: self.shown,
                    })
                };
                let feed = if tpl_info.list == Some(k) {
                    &*list_feed.get_or_insert_with(make)
                } else {
                    exp.children[k].get_or_init(make)
                };
                self.add(
                    &tpl,
                    self.templates.len(),
                    load,
                    depth + 1,
                    Parent {
                        slice,
                        index,
                        item,
                        instance: Some(i),
                        inputs: Some(&feed.inputs),
                        hashes: feed.hashes,
                        writes: nested_writes.get(k).map_or(&[][..], Vec::as_slice),
                        context: Some(&feed.context),
                        fed: &feed.fed,
                        bound: &feed.bound,
                        wheel_groups: &feed.wheel_groups,
                        components: &feed.components,
                        lists: lists.saturating_add(u8::from(list.is_some())),
                        started: &feed.started,
                        key: child_key,
                        sized,
                        hidden,
                        reuse: false,
                    },
                )
            };
            // The template root's children and components move onto the
            // instance: a layout component on the template root lays out
            // its widgets. The root, which nothing walks to, keeps its
            // copy for a later build to move the subtree over (`reuse`).
            let kids = self.nodes[sub].children.clone();
            for &c in &kids {
                self.parent[c] = Some(i);
            }
            let comps = self.nodes[sub].components.clone();
            self.insts[child].outputs_read =
                i - base < widgets.len() && table.output_sources.binary_search(&k).is_ok();
            let collapsed = self.insts[child].collapsed;
            // What the template's own bindings and clips write on its root
            // (Visible, AlphaTint, RotationAngle, Position) applies to the
            // instance that replaces it: a radial list item turns and places
            // itself from its slice (the diplomacy participant spokes).
            let root = self.nodes[sub].bound;
            let n = &mut self.nodes[i];
            if root.visible == Some(false) {
                n.geometry.visible = false;
            }
            if let Some(a) = root.alpha {
                n.geometry.alpha *= a;
            }
            if let Some(r) = root.rotation {
                n.geometry.rotation = r;
            }
            if let Some(p) = root.position {
                n.geometry.position = p;
            }
            n.children = kids;
            n.components.extend(comps);
            n.size = size;
            if fixed {
                n.intrinsic = n.size;
            } else if n.bound.size.is_some() && n.bound.scale.is_none() {
                // A bound Size scales a free-size container over its
                // content: the minimap's lode badge (154 px of artwork) is
                // sized to its label + 18. The content size is the previous
                // pass's, drawn at the stored scale. Size and Scale set the
                // same thing; where both are bound (the options button:
                // Scale 0.34, a Size of 5×28) Scale is kept.
                // TODO(alex): inferred from that badge; native code not read.
                let was = old.map(|o| o + i - base);
                self.reads.push(Read::Size(
                    i,
                    was.and_then(|w| self.sizes.get(w)).copied(),
                    Some(
                        was.and_then(|w| self.mins.get(w))
                            .copied()
                            .unwrap_or([f32::NAN; 2]),
                    ),
                ));
                let n = &mut self.nodes[i];
                let s = n.geometry.scale;
                n.origin = was
                    .and_then(|w| self.mins.get(w))
                    .copied()
                    .unwrap_or_default();
                n.intrinsic = was
                    .and_then(|w| self.sizes.get(w))
                    .filter(|z| z[0] > 0. && z[1] > 0.)
                    .map(|z| [z[0] / s[0].abs().max(1e-6), z[1] / s[1].abs().max(1e-6)]);
            }
            // Collapsed: its content is gone, its box stays.
            if let Some(b) = collapsed {
                let n = &mut self.nodes[i];
                n.intrinsic = Some(b.size());
                n.origin = b.min;
            }
        }
        // Against the previous build's block: which nodes are the same, and
        // whether any box moved (for skipping the size pass).
        if let (Some(o), Some(prev)) = (old, self.prev.as_deref())
            && o + (block_end - base) <= prev.nodes.len()
        {
            for k in 0..block_end - base {
                if same_node(&self.nodes[base + k], &prev.nodes[o + k]) {
                    self.prev_of[base + k] = prev.prev_of.get(o + k).copied().unwrap_or(NOT_KEPT);
                    self.pass_of[base + k] = (o + k) as u32;
                }
            }
        }
        if !self.boxes_moved {
            let len = block_end - base;
            self.boxes_moved = match (old, self.prev.as_deref()) {
                (Some(o), Some(prev)) if o + len <= prev.nodes.len() => !self.nodes
                    [base..block_end]
                    .iter()
                    .zip(&prev.nodes[o..o + len])
                    .all(|(a, b)| boxes_alike(a, b)),
                _ => true,
            };
        }
        let last = self.insts.len();
        let r = &mut self.insts[rec];
        r.last = last;
        r.end = self.nodes.len();
        r.files_end = self.templates.len();
        r.reads.1 = self.reads.len();
        base
    }

    /// Replaces each outermost subtree built under a hidden container (for
    /// its box or outputs only) by its template root, its container keeping
    /// the box it has in this settled layout, and renumbers `seed` (built
    /// in this tree's shape) to match. What a hidden subtree gives the rest
    /// depends only on what its container hands it, so the next layout
    /// takes it as it is when that is the same (see `add`): the transfer
    /// screen keeps 1.5k of its 22k nodes.
    fn collapse(&mut self, placed: &[Placed], seed: &mut Seed) -> bool {
        let mut cut: Vec<usize> = Vec::new();
        let mut r = 0;
        while r < self.insts.len() {
            let d = self.insts[r];
            // Only where something goes: a root alone stays as it is.
            if d.incoming != 0 && d.collapsed.is_none() && (d.end > d.base + 1 || d.last > r + 1) {
                cut.push(r);
                r = d.last;
            } else {
                r += 1;
            }
        }
        if cut.is_empty() || seed.sizes.len() != self.nodes.len() {
            return false;
        }
        // What stays, and where each position lands: `before[x]` counts what
        // stays below x, so it also maps one-past-the-end positions.
        let kept = |len: usize, gone: &mut dyn Iterator<Item = std::ops::Range<usize>>| {
            let mut out = vec![0u64; len.div_ceil(64)];
            for (i, w) in out.iter_mut().enumerate() {
                *w = if (i + 1) * 64 <= len {
                    u64::MAX
                } else {
                    (1u64 << (len % 64)) - 1
                };
            }
            for g in gone {
                for x in g {
                    out[x / 64] &= !(1 << (x % 64));
                }
            }
            let mut before = Vec::with_capacity(len + 1);
            let mut n = 0;
            for x in 0..len {
                before.push(n);
                n += (out[x / 64] >> (x % 64) & 1) as usize;
            }
            before.push(n);
            (out, before)
        };
        let on = |set: &[u64], x: usize| set[x / 64] >> (x % 64) & 1 == 1;
        let insts = &self.insts;
        let (keep_node, node) = kept(
            self.nodes.len(),
            &mut cut.iter().map(|&r| insts[r].base + 1..insts[r].end),
        );
        let (keep_rec, rec) = kept(insts.len(), &mut cut.iter().map(|&r| r + 1..insts[r].last));
        let (keep_read, read) = kept(
            self.reads.len(),
            &mut cut.iter().map(|&r| insts[r].reads.0..insts[r].reads.1),
        );
        let (keep_file, file) = kept(
            self.templates.len(),
            &mut cut.iter().map(|&r| insts[r].file..insts[r].files_end),
        );
        // File k is templates[k - 1]; a count of files maps like a position.
        let file_no = |f: usize| if f == 0 { 0 } else { file[f - 1] + 1 };
        let mut boxes: HashMap<usize, Rect> = HashMap::new();
        for &r in &cut {
            if let Some(c) = self.insts[r].container {
                boxes.insert(r, placed.get(c).map_or_else(Rect::default, |p| p.local));
            }
        }
        fn kept_of<'a, T: 'a>(v: Vec<T>, set: &'a [u64]) -> impl Iterator<Item = T> + 'a {
            v.into_iter()
                .enumerate()
                .filter(move |(j, _)| set[j / 64] >> (j % 64) & 1 == 1)
                .map(|(_, x)| x)
        }
        let remap = |x: Option<usize>| x.map(|x| node[x]);
        self.prev_of = kept_of(std::mem::take(&mut self.prev_of), &keep_node).collect();
        // Nothing is laid out against the build before any more.
        self.pass_of = vec![NOT_KEPT; self.prev_of.len()];
        self.nodes = kept_of(std::mem::take(&mut self.nodes), &keep_node)
            .map(|mut n| {
                n.children.retain(|&c| on(&keep_node, c));
                for c in &mut n.children {
                    *c = node[*c];
                }
                n
            })
            .collect();
        self.source = kept_of(std::mem::take(&mut self.source), &keep_node)
            .map(|(f, w)| (file_no(f), w))
            .collect();
        self.instance = kept_of(std::mem::take(&mut self.instance), &keep_node)
            .map(remap)
            .collect();
        self.parent = kept_of(std::mem::take(&mut self.parent), &keep_node)
            .map(remap)
            .collect();
        self.templates = kept_of(std::mem::take(&mut self.templates), &keep_file).collect();
        self.placed = self
            .placed
            .iter()
            .filter(|&&j| on(&keep_node, j))
            .map(|&j| node[j])
            .collect();
        self.reads = kept_of(std::mem::take(&mut self.reads), &keep_read)
            .map(|r| r.moved(|j| node[j]))
            .collect();
        let insts = std::mem::take(&mut self.insts);
        self.insts = insts
            .iter()
            .enumerate()
            .filter(|(r, _)| on(&keep_rec, *r))
            .map(|(r, d)| Inst {
                base: node[d.base],
                end: node[d.end],
                file: file_no(d.file),
                files_end: file[d.files_end],
                last: rec[d.last],
                reads: (read[d.reads.0], read[d.reads.1]),
                container: remap(d.container),
                collapsed: boxes.get(&r).copied().or(d.collapsed),
                ..*d
            })
            .collect();
        // A collapsed container's box is its own now: it has no content.
        for d in &self.insts {
            if let (Some(b), Some(c)) = (d.collapsed, d.container) {
                self.nodes[c].intrinsic = Some(b.size());
                self.nodes[c].origin = b.min;
            }
        }
        seed.sizes = kept_of(std::mem::take(&mut seed.sizes), &keep_node).collect();
        seed.transforms = kept_of(std::mem::take(&mut seed.transforms), &keep_node).collect();
        seed.mins = kept_of(std::mem::take(&mut seed.mins), &keep_node).collect();
        seed.bases = self
            .insts
            .iter()
            .enumerate()
            .map(|(r, d)| (d.key, (r, d.base)))
            .collect();
        seed.shape = self.insts.iter().map(|d| (d.key, d.base)).collect();
        self.prune_values();
        true
    }

    /// The container outputs this build's bindings read are the ones it
    /// made.
    fn outputs_settled(&self) -> bool {
        // A relayout seeded from an earlier one, after an
        // edit that changes outputs but no size, reads the earlier
        // outputs first. Only outputs a parent reads: others swing
        // between rounds (a diplomacy column between 308 and 310)
        // with nothing built from them. Floats as sizes: outputs
        // computed from sizes move as little as they do.
        let near = |a: &DefaultValue, b: &DefaultValue| {
            let f = |x: u32| f32::from_bits(x);
            let close = |x: u32, y: u32| x == y || (f(x) - f(y)).abs() < 0.01;
            match (a, b) {
                (DefaultValue::F32(x), DefaultValue::F32(y)) => close(*x, *y),
                (DefaultValue::Vec(x), DefaultValue::Vec(y)) => {
                    x.len() == y.len() && x.iter().zip(y).all(|(x, y)| close(*x, *y))
                }
                _ => a == b,
            }
        };
        let same_values = |r: Option<&Values>, o: Option<&Values>| match (r, o) {
            (Some(r), Some(o)) => {
                Arc::ptr_eq(r, o)
                    || (r.len() == o.len()
                        && o.iter().all(|(u, v)| r.get(u).is_some_and(|w| near(v, w))))
            }
            (r, o) => r.is_none() && o.is_none(),
        };
        self.insts.iter().filter(|d| d.outputs_read).all(|d| {
            let read = match &self.prev_outputs {
                Some(m) => m.get(&d.key),
                None => match self.overwritten.get(&d.key) {
                    Some(was) => was.as_ref(),
                    None => return true,
                },
            };
            same_values(read, self.outputs.get(&d.key))
        })
    }

    /// Starts from `o`'s per-instance values, which it overwrites for what
    /// it builds anew.
    fn take_values(&mut self, o: &mut Tree) {
        self.outputs = std::mem::take(&mut o.outputs);
        self.traces = std::mem::take(&mut o.traces);
        self.inputs_in = std::mem::take(&mut o.inputs_in);
    }

    /// Drops the values of instances this build no longer has.
    fn prune_values(&mut self) {
        let live: FastMap<u64, ()> = self.insts.iter().map(|d| (d.key, ())).collect();
        self.outputs.retain(|k, _| live.contains_key(k));
        self.traces.retain(|k, _| live.contains_key(k));
        self.inputs_in.retain(|k, _| live.contains_key(k));
    }

    /// The outputs this build's bindings read: a seed's, or the previous
    /// build's, which it then overwrote in part.
    fn read_outputs(&mut self) -> FastMap<u64, Values> {
        if let Some(m) = self.prev_outputs.take() {
            return m;
        }
        let mut m = self.outputs.clone();
        for (k, was) in &self.overwritten {
            match was {
                Some(v) => m.insert(*k, v.clone()),
                None => m.remove(k),
            };
        }
        m
    }

    /// Room for a build the size of `o`, which this one mostly copies:
    /// growing node lists and maps entry by entry was a third of a reuse.
    fn reserve_like(&mut self, o: &Tree) {
        let n = o.nodes.len();
        self.nodes.reserve(n);
        self.prev_of.reserve(n);
        self.pass_of.reserve(n);
        self.source.reserve(n);
        self.instance.reserve(n);
        self.parent.reserve(n);
        self.insts.reserve(o.insts.len());
        self.reads.reserve(o.reads.len());
        self.templates.reserve(o.templates.len());
        self.outputs.reserve(o.outputs.len());
        self.traces.reserve(o.traces.len());
        self.inputs_in.reserve(o.inputs_in.len());
        self.evals.reserve(o.insts.len());
    }

    /// Moves record `r` of the previous build and its subtree over as the
    /// instance `rec` was going to build (the same feed, nothing below
    /// changed), shifting node, file and record indices.
    fn reuse(&mut self, r: usize, rec: usize) {
        let mut prev = self.prev.take().expect("a previous build");
        let p = prev.insts[r];
        // The whole tree: hand its parts over instead of copying them.
        if r == 0 && rec == 0 && p.end == prev.nodes.len() {
            self.nodes = std::mem::take(&mut prev.nodes);
            self.prev_of = std::mem::take(&mut prev.prev_of);
            self.prev_of.resize(self.nodes.len(), NOT_KEPT);
            self.pass_of = (0..self.nodes.len() as u32).collect();
            self.source = std::mem::take(&mut prev.source);
            self.instance = std::mem::take(&mut prev.instance);
            self.parent = std::mem::take(&mut prev.parent);
            self.placed = std::mem::take(&mut prev.placed);
            self.templates = std::mem::take(&mut prev.templates);
            self.reads = std::mem::take(&mut prev.reads);
            self.insts = std::mem::take(&mut prev.insts);
            for d in &mut self.insts {
                d.fresh = false;
            }
            self.evals = self
                .insts
                .iter()
                .filter_map(|d| Some((d.key, self.prev_evals.get(&d.key)?.clone())))
                .collect();
            self.prev = Some(prev);
            return;
        }
        let Inst {
            base,
            file,
            container: instance,
            ..
        } = self.insts[rec];
        // The rest of the tree, where it was and larger than what comes
        // before it (an edit that rebuilt only the root): swap in what was
        // rebuilt and take the rest whole. The previous tree keeps its old front, which the
        // blocks around this one still compare against.
        if base == p.base
            && p.end - p.base >= base
            && file == p.file
            && rec == r
            && instance == p.container
            && self.nodes.len() == base
            && self.reads.len() == p.reads.0
            && p.end == prev.nodes.len()
            && p.last == prev.insts.len()
            && p.files_end == prev.templates.len()
            && p.reads.1 == prev.reads.len()
        {
            fn splice<T>(mine: &mut Vec<T>, theirs: &mut Vec<T>) {
                let n = mine.len();
                let mut all = std::mem::take(theirs);
                all[..n].swap_with_slice(&mut mine[..]);
                *theirs = std::mem::replace(mine, all);
            }
            splice(&mut self.nodes, &mut prev.nodes);
            splice(&mut self.prev_of, &mut prev.prev_of);
            // The front as rebuilt, then the rest where it was.
            self.pass_of
                .extend(self.pass_of.len() as u32..self.nodes.len() as u32);
            splice(&mut self.source, &mut prev.source);
            splice(&mut self.instance, &mut prev.instance);
            splice(&mut self.parent, &mut prev.parent);
            splice(&mut self.templates, &mut prev.templates);
            splice(&mut self.reads, &mut prev.reads);
            self.insts.pop();
            splice(&mut self.insts, &mut prev.insts);
            for d in &mut self.insts[rec..] {
                d.fresh = false;
            }
            prev.placed.retain(|&j| j >= base);
            prev.placed.extend(self.placed.drain());
            self.placed = std::mem::take(&mut prev.placed);
            for d in &self.insts[rec..] {
                if let Some(e) = self.prev_evals.get(&d.key) {
                    self.evals.insert(d.key, e.clone());
                }
            }
            self.prev = Some(prev);
            return;
        }
        let inside = |j: usize| (p.base..p.end).contains(&j);
        let shift = |j: usize| j - p.base + base;
        let fshift = |f: usize| f - p.file + file;
        for j in p.base..p.end {
            let mut n = std::mem::take(&mut prev.nodes[j]);
            for c in &mut n.children {
                *c = shift(*c);
            }
            self.nodes.push(n);
            self.prev_of
                .push(prev.prev_of.get(j).copied().unwrap_or(NOT_KEPT));
            self.pass_of.push(j as u32);
            let (f, w) = prev.source[j];
            self.source.push((fshift(f), w));
            // Its own widgets hang under the container outside; the
            // template root's children were moved onto it.
            self.instance.push(match prev.instance[j] {
                Some(x) if inside(x) => Some(shift(x)),
                _ => instance,
            });
            self.parent
                .push(prev.parent[j].map(|x| if inside(x) { shift(x) } else { base }));
            if !prev.placed.is_empty() && prev.placed.contains(&j) {
                self.placed.insert(shift(j));
            }
        }
        self.templates
            .extend(prev.templates[p.file..p.files_end].iter().cloned());
        let reads = self.reads.len();
        self.reads.extend(
            prev.reads[p.reads.0..p.reads.1]
                .iter()
                .map(|r| r.moved(shift)),
        );
        for d in r..p.last {
            let q = prev.insts[d];
            if let Some(e) = self.prev_evals.get(&q.key) {
                self.evals.insert(q.key, e.clone());
            }
            let moved = Inst {
                base: shift(q.base),
                end: shift(q.end),
                file: fshift(q.file),
                files_end: fshift(q.files_end),
                last: q.last - r + rec,
                reads: (q.reads.0 - p.reads.0 + reads, q.reads.1 - p.reads.0 + reads),
                container: q.container.map(|c| if d == r { c } else { shift(c) }),
                fresh: false,
                ..q
            };
            if d == r {
                self.insts[rec] = Inst {
                    container: instance,
                    ..moved
                };
            } else {
                self.insts.push(moved);
            }
        }
        self.prev = Some(prev);
    }
}

const PROP_VIDEO: u32 = name_crc(b"Video");
const PROP_RESOLUTION: u32 = name_crc(b"Resolution");

// A VideoWidgetInstance's box is its VideoAsset's `Resolution` (160×160 on
// the main title video, scaled 12 × 6.75 to fill the screen).
fn video_size(w: &Widget, load: &mut dyn FnMut(Guid) -> Option<Arc<Asset>>) -> Option<[f32; 2]> {
    let video = w.props.iter().find_map(|p| match (p.crc, &p.value) {
        (PROP_VIDEO, Value::Ref(r)) => Some(r.id),
        _ => None,
    })?;
    let a = load(video)?;
    let (root, _) = a.objects().next()?;
    a.object_props(root)
        .into_iter()
        .find_map(|p| match (p.crc, p.value) {
            (PROP_RESOLUTION, Value::Vec2(v)) => Some(v),
            _ => None,
        })
}

// The template bool `0x072d960f` (+0x124) sets the instance's "fixed size"
// flag: its bounds are then its Size instead of its
// children's union.
fn uses_reference_size(tpl: &Asset) -> bool {
    let Some((root, _)) = tpl.objects().next() else {
        return false;
    };
    tpl.object_props(root)
        .iter()
        .any(|p| p.crc == PROP_USES_REFERENCE_SIZE && p.value == Value::Bool(true))
}

/// Builds the layout tree of one asset: node `k` is `widgets[k]` (index 0 =
/// the root). Intrinsic sizes the file itself knows are filled in (a
/// container's own `ReferenceSize` is not: that lives in the template the
/// caller loads); everything else is None.
pub fn layout_nodes(asset: &Asset, widgets: &[Widget]) -> Vec<LayoutNode> {
    widgets
        .iter()
        .map(|w| LayoutNode {
            geometry: asset.geometry(w).unwrap_or_default(),
            intrinsic: label_size(w),
            origin: [0.; 2],
            size: None,
            components: asset
                .components(w)
                .iter()
                .filter_map(LayoutComponent::from_component)
                .collect(),
            children: w.children.clone(),
            bound: Default::default(),
            text: None,
            loc: None,
            item: 0,
        })
        .collect()
}

const CLASS_LABEL: u32 = name_crc(b"LabelWidgetInstance");
const PROP_DISPLAYED_SIZE: u32 = name_crc(b"DisplayedSize");

// Labels store the size the editor measured their text at.
fn label_size(w: &Widget) -> Option<[f32; 2]> {
    if !is_a(w.class, CLASS_LABEL) {
        return None;
    }
    w.props.iter().find_map(|p| match (p.crc, &p.value) {
        (PROP_DISPLAYED_SIZE, Value::Vec2(v)) => Some(*v),
        _ => None,
    })
}

#[cfg(test)]
mod label_tests {
    use super::{label_box, label_origin, wrap};
    use crate::phx::LabelText;

    #[test]
    fn wrap_breaks_at_spaces() {
        let mut len = |t: &str| t.len() as f32;
        assert_eq!(wrap("one two three", 8., &mut len), ["one two", "three"]);
        assert_eq!(wrap("unbreakable", 3., &mut len), ["unbreakable"]);
    }

    // One glyph = 1 px, 10 px font: lines are 12.5 px.
    #[test]
    fn label_boxes_follow_sizing_and_alignment() {
        let mut len = |t: &str| t.len() as f32;
        let wrapping = LabelText {
            overflow: [1, 4],
            sizing: [0, 1],
            line_spacing: 1.,
            ..LabelText::default()
        };
        let (lines, size) = label_box(&wrapping, "one two three", 12.5, [8., 0.], &mut len);
        assert_eq!(lines, ["one two", "three"]);
        assert_eq!(size, [8., 25.]);
        let centred = LabelText {
            overflow: [5, 4],
            sizing: [1, 1],
            alignment: [1, 3],
            line_spacing: 1.,
            ..LabelText::default()
        };
        let (_, size) = label_box(&centred, "abcd", 12.5, [10., 0.], &mut len);
        assert_eq!(size, [4., 12.5]);
        assert_eq!(label_origin(&centred, [10., 0.], size), [3., -12.5]);
    }
}

#[cfg(test)]
mod wheel_tests {
    use super::{WHEEL_ALIGN_CENTER, WheelGroup, wheel_slices, wheel_step};

    fn group(begin: i32, min_items: i32) -> WheelGroup {
        WheelGroup {
            begin,
            min_items,
            begin_padding: 0.,
            end_padding: 5.,
            radius_offset: 0.,
            alignment: 0,
        }
    }

    // The in-game minimap ring (2026-11-04 screenshot): day/night, four
    // speeds, three controls, two lodes, six fertilities in eight slots,
    // starting one step before 138°. Measured centres, ±3°.
    #[test]
    fn wheel_groups_place_the_minimap_ring_as_in_game() {
        let groups = [
            group(0, 1),
            group(1, 4),
            group(5, 1),
            group(8, 2),
            group(10, 8),
        ];
        let step = wheel_step(360., &groups, 16);
        assert!((step - 335. / 18.).abs() < 1e-4);
        let slices = wheel_slices(138. - step, 360., 100., &groups, 16, false);
        let measured = [
            152., 171., 189., 207., 232., 251., 270., 293., 314., 337., 354., 373., 390., 408.,
            427.,
        ];
        for (i, m) in measured.iter().enumerate() {
            let (a, size, _) = slices[i + 1].unwrap();
            assert!((a - m).abs() < 3., "item {}: {a} vs {m}", i + 1);
            assert_eq!(size, step);
        }
    }

    #[test]
    fn windowed_wheels_move_back_half_a_step() {
        let slices = wheel_slices(0., 360., 10., &[], 4, true);
        let angles: Vec<f32> = slices.iter().map(|s| s.unwrap().0).collect();
        assert_eq!(angles, [0., 90., 180., 270.]);
    }

    #[test]
    fn wheel_without_groups_spreads_items_evenly() {
        let slices = wheel_slices(0., 360., 10., &[], 4, false);
        let angles: Vec<f32> = slices.iter().map(|s| s.unwrap().0).collect();
        assert_eq!(angles, [45., 135., 225., 315.]);
        let (_, _, p) = slices[0].unwrap();
        assert!((p[0] - 7.071).abs() < 1e-3 && (p[1] + 7.071).abs() < 1e-3);
    }

    #[test]
    fn wheel_alignment_and_uncovered_items() {
        // A centred group of four slots holding two items starts them one
        // slot in.
        let mut g = group(0, 4);
        g.end_padding = 0.;
        g.alignment = WHEEL_ALIGN_CENTER;
        let slices = wheel_slices(0., 40., 100., &[g], 2, false);
        assert_eq!(slices[0].unwrap().0, 15.);
        assert_eq!(slices[1].unwrap().0, 25.);
        // The last group spans the items left over, whatever its
        // BeginIndex; an item before the first group gets no slice.
        g.begin = 1;
        let slices = wheel_slices(0., 40., 100., &[g], 3, false);
        assert!(slices[0].is_none());
        assert_eq!(slices[1].unwrap().0, 5.);
    }
}
