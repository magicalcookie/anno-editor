// Widget views over a typed asset: the widget tree, per-widget properties,
// layout geometry, and what an ImageAsset / ImageAtlasAsset points at.
//
// Tree model (all 733 shipped assets): widgets never contain widgets.
// The root ContainerWidgetTemplate is a GroupTemplate whose `WidgetInstances`
// array holds the top-level widgets; nested `Groups` hold further
// GroupTemplates (`02 02 <id>`). A GroupInstance widget references one of
// those by id (each GroupTemplate is referenced exactly once), and that
// group's widgets are its children. ContainerWidgetInstance children live in
// the referenced template asset (`Template`), not in this file.

use std::collections::HashMap;

use std::ops::Range;

use anyhow::{Context, Result, bail, ensure};

use super::{
    Asset, Class, DefaultValue, Field, Guid, ObjectId, Prop, Record, Reference, Splice, Value, is_a,
};

/// One property of an object as the editor shows it.
#[derive(Debug, Clone, PartialEq)]
pub struct PropValue {
    pub name: Option<&'static str>,
    /// crc32 of the property name.
    pub crc: u32,
    /// Wire property index (presence-mask bit).
    prop: u16,
    /// First span of the value; for [`Value`] scalars the span to edit.
    pub span: usize,
    pub value: Value,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Widget {
    /// Object header span.
    pub object: usize,
    pub class: u32,
    /// The widget's name string (the `NameCrc`-prefixed one), if set.
    pub name: Option<String>,
    /// Index into the [`Asset::widgets`] list.
    pub parent: Option<usize>,
    /// Indices into the [`Asset::widgets`] list, in draw order.
    pub children: Vec<usize>,
    pub props: Vec<PropValue>,
}

/// WidgetInstance layout fields, with the constructor defaults
/// for absent ones.
///
/// Placement in the parent: `p' = anchor + position + R(rotation) · (scale ⊙
/// (p − pivot))` for a point `p` in the widget's intrinsic pixel space
/// (the full rules are in `layout`). Units are reference pixels (the root
/// template's `ReferenceSize`, 1920 × 1080 in shipped screens); rotation is
/// degrees.
/// The drawn size is `scale.xy × intrinsic size`: the ImageAsset `Size` for
/// image widgets, the template's `ReferenceSize` for container instances,
/// the child bounds for groups.
#[derive(Debug, Clone, PartialEq)]
pub struct Geometry {
    pub position: [f32; 3],
    pub pivot: [f32; 3],
    pub rotation: f32,
    pub scale: [f32; 3],
    /// `AlphaTint`, 0..1.
    pub alpha: f32,
    /// `ColorTint` RGB, when set.
    pub tint: Option<[u8; 3]>,
    pub visible: bool,
    pub hidden: bool,
    /// `AnchorPoint` @0xa0: 0 top-left … 8 bottom-right of the parent's
    /// Size (see `layout`).
    pub anchor: u32,
    /// x, y: `0x5c63f4bb` / `0xc16c15cd` @0xa4/0xa8 (legacy names
    /// Horizontal/VerticalAnchorMetrics). 1/2 divide Position by the
    /// ancestors' / the parent's scale.
    pub anchor_metrics: [u32; 2],
    /// x, y: `WidthResizing` / `HeightResizing` @0xac/0xb0 (legacy
    /// Horizontal/VerticalResizeMetrics). 1/2 cancel the ancestors' / the
    /// parent's scale on this widget.
    pub resize_metrics: [u32; 2],
    /// `AspectRatio` @0xb4: non-zero makes the ancestors' scale uniform on
    /// this widget (1 the smaller axis, 2 x, 3 y); overrides `resize_metrics`.
    pub aspect_ratio: u32,
    /// `ZIndex`.
    pub z_index: u32,
    /// `ConsiderSize`: counts towards its parent's bounds (default true;
    /// off on drop shadows, oversized masked patterns).
    pub consider_size: bool,
    /// `EnumEntry`: the style of an enum widget (WidgetType with
    /// `CurrentEnumValue`, id 0x30000004) this widget exists in; 0 in all.
    pub enum_entry: u32,
}

impl Default for Geometry {
    fn default() -> Geometry {
        Geometry {
            position: [0.0; 3],
            pivot: [0.0; 3],
            rotation: 0.0,
            scale: [1.0; 3],
            alpha: 1.0,
            tint: None,
            visible: true,
            hidden: false,
            anchor: 0,
            anchor_metrics: [0; 2],
            resize_metrics: [0; 2],
            aspect_ratio: 0,
            z_index: 0,
            consider_size: true,
            enum_entry: 0,
        }
    }
}

/// What an image widget shows: an ImageAsset reference (resolve it with
/// [`image_info`] on `data/ui/studio/images/<guid>`) and the UV transform.
#[derive(Debug, Clone, PartialEq)]
pub struct WidgetImage {
    pub asset: Option<Reference>,
    pub uv_offset: [f32; 2],
    pub uv_scale: [f32; 2],
    pub uv_rotation: f32,
    /// FillMode @0x118: 0 stretches the image over its box, 1 fits it
    /// (scaled to keep its aspect, centred), 2 covers it.
    pub fill: u32,
    /// The unnamed string @0xf0 (`0x2c697b67`): a texture path the widget
    /// draws instead of its Image's, as a binding's image path (builtin 42)
    /// does. 535 of the 618 shipped ones sit on the `null_16` placeholder
    /// ImageAsset (the minimap's wind arrows, close crosses, jump-to icons).
    pub path: Option<String>,
}

/// What a label widget shows.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct LabelText {
    pub text: String,
    pub displayed_text: String,
    pub localization_id: String,
    /// Font asset (class `0xe1523167`, the `PHXLG` kind root).
    pub font: Option<Reference>,
    /// TextStyle asset (`PHXTS`).
    pub style: Option<Reference>,
    /// `AlignmentX`, `AlignmentY` values.
    pub alignment: [u32; 2],
    /// The box text lays out in (pixels; 0 = none set).
    pub text_plane: [f32; 2],
    /// HorizontalOverflow / VerticalOverflow (enums `fb1bc8b6…` /
    /// `7584ee8b…`): horizontally 1 wraps inside the TextPlane width, 5
    /// shows everything; vertically 4 shows every line.
    pub overflow: [u32; 2],
    /// Horizontal/VerticalSizing (TextBoxSizing `b761da4f…`): the label's
    /// size per axis is 0 the TextPlane, 1 the displayed text, 2 the larger
    /// of both, 3 the smaller.
    pub sizing: [u32; 2],
    /// MaxLineCount (0: no limit) and LineSpacing (a factor).
    pub max_lines: f32,
    pub line_spacing: f32,
}

/// What an ImageAsset (`PHXIM`) draws.
#[derive(Debug, Clone, PartialEq)]
pub struct ImageInfo {
    /// Texture path (`data\ui\…\x.png`), property `ImageId`.
    pub path: String,
    /// Pixel size; the intrinsic size image widgets scale.
    pub size: [f32; 2],
    /// 9-slice centre `[TopLeft, BottomRight]` in 0..1 of the image: the
    /// part that stretches; the borders keep their pixel size. `(0,0)-(1,1)`
    /// (the default) means no slicing. Pixel margins: `TopLeft * size` and
    /// `(1 - BottomRight) * size`.
    pub slice: [[f32; 2]; 2],
    /// `TextureFiltering` enum value.
    filtering: u32,
    /// The ImageAtlasAsset this image is a region of, if any.
    pub(crate) atlas: Option<Guid>,
}

/// What a widget's own fields say. What it owns (components, effects,
/// property overrides) is read through its [`Asset`].
impl Widget {
    /// Layout fields of a WidgetInstance; None for the root and non-widgets.
    pub fn geometry(&self) -> Option<Geometry> {
        if !is_a(self.class, Class::WIDGET_INSTANCE) {
            return None;
        }
        let mut g = Geometry::default();
        for p in &self.props {
            match (p.crc, &p.value) {
                (Prop::POSITION, Value::Vec3(v)) => g.position = *v,
                (Prop::PIVOT, Value::Vec3(v)) => g.pivot = *v,
                (Prop::ROTATION, Value::F32(v)) => g.rotation = *v,
                (Prop::SCALE, Value::Vec3(v)) => g.scale = *v,
                (Prop::ALPHA_TINT, Value::F32(v)) => g.alpha = *v,
                (Prop::COLOR_TINT, Value::Color(c)) => g.tint = Some(*c),
                (Prop::VISIBLE, Value::Bool(v)) => g.visible = *v,
                (Prop::IS_HIDDEN, Value::Bool(v)) => g.hidden = *v,
                (Prop::ANCHOR, Value::Enum { value, .. }) => g.anchor = *value,
                (Prop::Z_INDEX, Value::U32(v)) => g.z_index = *v,
                (Prop::ENUM_ENTRY, Value::U32(v)) => g.enum_entry = *v,
                (Prop::CONSIDER_SIZE, Value::Bool(v)) => g.consider_size = *v,
                (Prop::ASPECT_RATIO, Value::Enum { value, .. }) => g.aspect_ratio = *value,
                (c, Value::Enum { value, .. }) if Prop::ANCHOR_METRICS.contains(&c) => {
                    g.anchor_metrics[usize::from(c == Prop::ANCHOR_METRICS[1])] = *value;
                }
                (c, Value::Enum { value, .. }) if Prop::RESIZE_METRICS.contains(&c) => {
                    g.resize_metrics[usize::from(c == Prop::RESIZE_METRICS[1])] = *value;
                }
                _ => {}
            }
        }
        Some(g)
    }

    /// A container instance's stored `CurrentEnumValue`: the style of its
    /// template it starts in (0 when absent).
    pub fn enum_value(&self) -> u32 {
        self.props
            .iter()
            .find_map(|p| match (p.crc, &p.value) {
                (Prop::CURRENT_ENUM_VALUE, Value::U32(v)) => Some(*v),
                _ => None,
            })
            .unwrap_or(0)
    }

    /// The ContainerWidgetTemplate asset a ContainerWidgetInstance expands
    /// to (`data/ui/studio/generated/<guid>`); its widgets are this widget's
    /// children.
    pub fn container_template(&self) -> Option<Reference> {
        if !is_a(self.class, Class::CONTAINER_INSTANCE) {
            return None;
        }
        self.props.iter().find_map(|p| match p {
            PropValue {
                crc: Prop::TEMPLATE,
                value: Value::Ref(r),
                ..
            } => Some(*r),
            _ => None,
        })
    }

    /// The image of an ImageWidgetInstance.
    pub fn image(&self) -> Option<WidgetImage> {
        if !is_a(self.class, Class::IMAGE_WIDGET) {
            return None;
        }
        let mut img = WidgetImage {
            asset: None,
            uv_offset: [0.0; 2],
            uv_scale: [1.0; 2],
            uv_rotation: 0.0,
            fill: 0,
            path: None,
        };
        for p in &self.props {
            match (p.crc, &p.value) {
                (Prop::IMAGE, Value::Ref(r)) => img.asset = Some(*r),
                (Prop::FILL_MODE, Value::Enum { value, .. }) => img.fill = *value,
                (Prop::UV_OFFSET, Value::Vec2(v)) => img.uv_offset = *v,
                (Prop::UV_SCALE, Value::Vec2(v)) => img.uv_scale = *v,
                (Prop::UV_ROTATION, Value::F32(v)) => img.uv_rotation = *v,
                (Prop::IMAGE_PATH, Value::Str(s)) if !s.is_empty() => img.path = Some(s.clone()),
                _ => {}
            }
        }
        Some(img)
    }

    /// The text of a LabelWidgetInstance.
    pub fn label(&self) -> Option<LabelText> {
        if !is_a(self.class, Class::LABEL_WIDGET) {
            return None;
        }
        let mut l = LabelText {
            line_spacing: 1.,
            // Overflow "Visible" and sizing by the displayed text: a label
            // shows its text, one line, as wide as it is.
            overflow: [5, 4],
            sizing: [1, 1],
            ..LabelText::default()
        };
        for p in &self.props {
            match (p.crc, &p.value) {
                (Prop::TEXT, Value::Str(s)) => l.text = s.clone(),
                (Prop::DISPLAYED_TEXT, Value::Str(s)) => l.displayed_text = s.clone(),
                (Prop::LOCALIZATION_ID, Value::Str(s)) => l.localization_id = s.clone(),
                (Prop::FONT, Value::Ref(r)) => l.font = Some(*r),
                (Prop::TEXT_STYLE, Value::Ref(r)) => l.style = Some(*r),
                (c, Value::U32(v)) if Prop::ALIGNMENT.contains(&c) => {
                    l.alignment[usize::from(c == Prop::ALIGNMENT[1])] = *v;
                }
                (c, Value::U32(v)) if Prop::OVERFLOW.contains(&c) => {
                    l.overflow[usize::from(c == Prop::OVERFLOW[1])] = *v;
                }
                (c, Value::U32(v)) if Prop::SIZING.contains(&c) => {
                    l.sizing[usize::from(c == Prop::SIZING[1])] = *v;
                }
                (Prop::TEXT_PLANE, Value::Vec2(v)) => l.text_plane = *v,
                (Prop::MAX_LINES, Value::F32(v)) => l.max_lines = *v,
                (Prop::LINE_SPACING, Value::F32(v)) => l.line_spacing = *v,
                _ => {}
            }
        }
        Some(l)
    }
}

impl Asset {
    /// Every object span grouped by its owning field, first span per field.
    pub(crate) fn fields(&self) -> super::Fields<'_> {
        super::Fields(&self.field_spans)
    }

    /// The properties present on the object whose header is span `object`,
    /// in wire order.
    pub fn object_props(&self, object: usize) -> Vec<PropValue> {
        self.props_with(object, &self.fields())
    }

    fn props_with(&self, object: usize, fields: &super::Fields) -> Vec<PropValue> {
        let Record::Object(header) = &self.spans[object].record else {
            return Vec::new();
        };
        let Some(props) = self.class_props(header.class) else {
            return Vec::new();
        };
        (1..header.prop_count as u16)
            .filter_map(|i| {
                let span = fields.get(&Field { object, prop: i })?;
                let p = props[i as usize - 1];
                Some(PropValue {
                    name: p.name,
                    crc: p.name_crc,
                    prop: i,
                    span,
                    value: self.span_value(span, fields),
                })
            })
            .collect()
    }

    // The editor-facing value of a property whose first span is `span`.
    fn span_value(&self, span: usize, fields: &super::Fields) -> Value {
        match &self.spans[span].record {
            Record::Value(v) => v.clone(),
            Record::Str(s) => Value::Str(s.text.clone()),
            Record::Ref(r) => Value::Ref(*r),
            Record::Null => Value::Null,
            Record::Color(rgb) => Value::Color(*rgb),
            Record::Object(o) if o.class == Class::COLOR => fields
                .get(&Field {
                    object: span,
                    prop: 1,
                })
                .map_or(Value::Other, |c| self.span_value(c, fields)),
            // A ColorType: its palette entry when that palette is known,
            // else the RGB of its embedded Color.
            Record::Object(o) if o.class == Class::COLOR_TYPE => {
                let props = self.props_with(span, fields);
                let palette = props.iter().find_map(|p| match p.value {
                    Value::Ref(r) => Some(r.id),
                    _ => None,
                });
                let entry = props.iter().find_map(|p| match p.value {
                    Value::U32(v) => Some(v),
                    _ => None,
                });
                if let (Some(g), Some(e)) = (palette, entry)
                    && let Some(rgb) = self.palette_color(g, e)
                {
                    return Value::Color(rgb);
                }
                props
                    .into_iter()
                    .find_map(|p| match p.value {
                        Value::Color(_) => Some(p.value),
                        _ => None,
                    })
                    .unwrap_or(Value::Other)
            }
            _ => Value::Other,
        }
    }

    /// The widget tree; index 0 is the root template. Empty for assets that
    /// are not widget templates or were not fully typed. Read again when
    /// the asset's palettes change ([`Asset::use_palettes`], or one more
    /// registered: ColorType values resolve through them); layout asks for
    /// it per template and per container instance.
    pub fn widgets(&self) -> std::sync::Arc<[Widget]> {
        let key = self.palette_key();
        let mut cached = self.widgets.read.lock().expect("widget cache");
        if let Some((k, w)) = &*cached
            && *k == key
        {
            return w.clone();
        }
        let w: std::sync::Arc<[Widget]> = self.read_widgets().into();
        *cached = Some((key, w.clone()));
        w
    }

    fn read_widgets(&self) -> Vec<Widget> {
        let Some((root, root_header)) = self.objects().next() else {
            return Vec::new();
        };
        if !self.typed || !is_a(root_header.class, Class::GROUP_TEMPLATE) {
            return Vec::new();
        }
        let fields = self.fields();
        let name_of = |o: usize| {
            self.props_with(o, &fields)
                .into_iter()
                .find(|p| p.crc == Prop::NAME)
                .and_then(|p| match p.value {
                    Value::Str(s) => Some(s),
                    _ => None,
                })
        };
        // Widget objects, keyed by the group (root or GroupTemplate) that
        // holds them in its WidgetInstances array.
        let mut by_group: HashMap<usize, Vec<usize>> = HashMap::new();
        let mut group_by_id: HashMap<Guid, usize> = HashMap::new();
        for (i, o) in self.objects() {
            if let ObjectId::Id(id) = o.id
                && is_a(o.class, Class::GROUP_TEMPLATE)
            {
                group_by_id.insert(id, i);
            }
            if !is_a(o.class, Class::WIDGET_INSTANCE) {
                continue;
            }
            if let Some(f) = self.spans[i].field
                && self.prop_crc(f) == Some(Prop::WIDGET_INSTANCES)
            {
                by_group.entry(f.object).or_default().push(i);
            }
        }

        let mut out = vec![Widget {
            object: root,
            class: root_header.class,
            name: name_of(root),
            parent: None,
            children: Vec::new(),
            props: self.props_with(root, &fields),
        }];
        // Breadth-first from the root: a widget's children are its group's
        // widgets (root: its own; GroupInstance: the referenced group's).
        let mut queue = vec![(0usize, root)];
        while let Some((w, group)) = queue.pop() {
            for &o in by_group.get(&group).into_iter().flatten() {
                let Record::Object(h) = &self.spans[o].record else {
                    continue;
                };
                let props = self.props_with(o, &fields);
                let idx = out.len();
                if h.class == Class::GROUP_INSTANCE
                    && let Some(Value::Ref(r)) = props
                        .iter()
                        .find(|p| p.crc == Prop::GROUP_TEMPLATE)
                        .map(|p| &p.value)
                    && let Some(&g) = group_by_id.get(&r.id)
                {
                    queue.push((idx, g));
                }
                out.push(Widget {
                    object: o,
                    class: h.class,
                    name: name_of(o),
                    parent: Some(w),
                    children: Vec::new(),
                    props,
                });
                out[w].children.push(idx);
            }
        }
        out
    }

    fn prop_crc(&self, f: Field) -> Option<u32> {
        let Record::Object(h) = &self.spans[f.object].record else {
            return None;
        };
        Some(
            self.class_props(h.class)?
                .get(f.prop as usize - 1)?
                .name_crc,
        )
    }

    /// The root template's `ReferenceSize`: the pixel size its widgets are
    /// laid out in (1920 × 1080 for screens and most templates).
    pub fn reference_size(&self) -> Option<[f32; 2]> {
        let (root, h) = self.objects().next()?;
        if !is_a(h.class, Class::CONTAINER_TEMPLATE) {
            return None;
        }
        self.object_props(root).into_iter().find_map(|p| match p {
            PropValue {
                crc: Prop::REFERENCE_SIZE,
                value: Value::Vec2(v),
                ..
            } => Some(v),
            _ => None,
        })
    }

    /// The root's `WidgetType` (a `PHXBE` WidgetType asset: native widget
    /// behaviour such as TextInput, lists, enum styles), or for a WidgetType
    /// asset its `ParentWidgetType`.
    pub fn widget_type(&self) -> Option<Reference> {
        let (root, _) = self.objects().next()?;
        self.object_props(root).into_iter().find_map(|p| match p {
            PropValue {
                crc: Prop::WIDGET_TYPE | Prop::PARENT_WIDGET_TYPE,
                value: Value::Ref(r),
                ..
            } => Some(r),
            _ => None,
        })
    }

    /// The template's WidgetType chain, its own first (a Button's, then
    /// InteractiveWidget's…), each type loaded through `load`; it ends where
    /// `load` finds nothing. Layout asks per instance, and the chain never
    /// changes, so the first answer is kept on the asset. It stays an
    /// asset's because the chain is its install's: 117 and 1800 share
    /// guids, their palette's among them.
    pub fn widget_types(
        &self,
        load: &mut dyn FnMut(Guid) -> Option<std::sync::Arc<Asset>>,
    ) -> &[Guid] {
        self.widget_types.get_or_init(|| {
            let mut out = Vec::new();
            let mut ty = self.widget_type().map(|r| r.id);
            // Chains are a handful deep; the bound guards a cycle.
            while let Some(t) = ty {
                out.push(t);
                if out.len() == 8 {
                    break;
                }
                ty = load(t).and_then(|a| a.widget_type()).map(|r| r.id);
            }
            out.into_boxed_slice()
        })
    }
}

// ---------------------------------------------------------------------------
// Tree edits
//
// A widget is its object in a group's `WidgetInstances` array; a
// GroupInstance also owns the GroupTemplate it references, which sits in
// the `Groups` array of the group holding the instance (root or nested
// GroupTemplate; 2,082/2,082 installed) and holds the children and their
// own groups. Edits splice those byte ranges and fix the array counts;
// `Asset::rebuild` re-derives the class table and heap section.

/// A widget cut out of a file: its object bytes, and for a GroupInstance
/// the GroupTemplate subtree with all its descendants.
#[derive(Debug, Clone)]
pub struct WidgetCopy {
    widget: Vec<u8>,
    group: Option<Vec<u8>>,
    /// Every class of every object inside, for the class table.
    classes: Vec<u32>,
    /// Ids of the objects inside; [`Asset::insert_widget`] can replace them.
    ids: Vec<Guid>,
    pub name: Option<String>,
}

/// A removed widget: the new asset, and how many references from outside
/// the removed part still name one of its objects (data-binding paths of
/// the root's `0x8fda299c` table, mostly). The game resolves them at load;
/// what it does with a missing target is not known.
// TODO(alex): drop or retarget bindings whose path ends in a removed widget
// once the binding index fields are understood.
#[derive(Debug, Clone)]
pub struct Removed {
    pub asset: Asset,
    pub dangling: usize,
}

impl Asset {
    /// Span index one past the last span of the object at `object`: its
    /// values and every object nested in them.
    pub(super) fn subtree_end(&self, object: usize) -> usize {
        (object + 1..self.spans.len())
            .find(|&i| self.spans[i].field.is_none_or(|f| f.object < object))
            .unwrap_or(self.spans.len())
    }

    pub(super) fn subtree_bytes(&self, object: usize) -> Range<usize> {
        let end = self.subtree_end(object);
        self.spans[object].range.start..self.spans[end - 1].range.end
    }

    /// An array property of an object: its count span and element objects.
    pub(super) fn array(&self, object: usize, crc: u32) -> Option<(usize, Vec<usize>)> {
        let Record::Object(h) = &self.spans[object].record else {
            return None;
        };
        let prop = self
            .class_props(h.class)?
            .iter()
            .position(|p| p.name_crc == crc)?
            + 1;
        let field = Field {
            object,
            prop: prop as u16,
        };
        let end = self.subtree_end(object);
        let count = (object + 1..end).find(|&i| {
            self.spans[i].field == Some(field) && matches!(self.spans[i].record, Record::Count(_))
        })?;
        let elements = (count + 1..end)
            .filter(|&i| {
                self.spans[i].field == Some(field)
                    && matches!(self.spans[i].record, Record::Object(_))
            })
            .collect();
        Some((count, elements))
    }

    /// Byte offset where element `index` of an array starts (the end of the
    /// array when `index` is past it).
    pub(super) fn array_slot(&self, count: usize, elements: &[usize], index: usize) -> usize {
        match elements.get(index) {
            Some(&e) => self.spans[e].range.start,
            None => elements
                .last()
                .map_or(self.spans[count].range.end, |&e| self.subtree_bytes(e).end),
        }
    }

    fn object_by_id(&self, id: Guid) -> Option<usize> {
        self.objects()
            .find(|(_, o)| o.id == ObjectId::Id(id))
            .map(|(i, _)| i)
    }

    /// The group object holding widget `w`'s children: the root template
    /// for the root, the referenced GroupTemplate for a GroupInstance. None
    /// for widgets that cannot have children here (images, labels, and
    /// containers, whose children live in their template file).
    pub fn child_group(&self, widgets: &[Widget], w: usize) -> Option<usize> {
        let widget = widgets.get(w)?;
        if w == 0 {
            return Some(widget.object);
        }
        if widget.class != Class::GROUP_INSTANCE {
            return None;
        }
        widget.props.iter().find_map(|p| match (p.crc, &p.value) {
            (Prop::GROUP_TEMPLATE, Value::Ref(r)) => self.object_by_id(r.id),
            _ => None,
        })
    }

    /// Copies widget `w` (index into `widgets`) with everything under it.
    pub fn widget_copy(&self, widgets: &[Widget], w: usize) -> Result<WidgetCopy> {
        ensure!(w > 0 && w < widgets.len(), "the root cannot be copied");
        let object = widgets[w].object;
        let group = if widgets[w].class == Class::GROUP_INSTANCE {
            Some(
                self.child_group(widgets, w)
                    .context("the group's template is missing")?,
            )
        } else {
            None
        };
        let mut classes = Vec::new();
        let mut ids = Vec::new();
        for root in std::iter::once(object).chain(group) {
            for i in root..self.subtree_end(root) {
                if let Record::Object(o) = &self.spans[i].record {
                    if !classes.contains(&o.class) {
                        classes.push(o.class);
                    }
                    if let ObjectId::Id(g) = o.id {
                        ids.push(g);
                    }
                }
            }
        }
        Ok(WidgetCopy {
            widget: self.bytes[self.subtree_bytes(object)].to_vec(),
            group: group.map(|g| self.bytes[self.subtree_bytes(g)].to_vec()),
            classes,
            ids,
            name: widgets[w].name.clone(),
        })
    }

    /// Inserts `copy` as child `index` of widget `parent`. With `fresh`, every
    /// object id inside gets a new random id (and references inside the copy
    /// follow), so the same copy can be inserted next to its original.
    pub fn insert_widget(
        &self,
        widgets: &[Widget],
        parent: usize,
        index: usize,
        copy: &WidgetCopy,
        fresh: bool,
    ) -> Result<Asset> {
        let group = self
            .child_group(widgets, parent)
            .context("only the root and groups can hold widgets")?;
        let (mut widget, mut group_bytes) = (copy.widget.clone(), copy.group.clone());
        if fresh {
            for id in &copy.ids {
                let new = Guid::random();
                replace_all(&mut widget, &id.0, &new.0);
                if let Some(g) = &mut group_bytes {
                    replace_all(g, &id.0, &new.0);
                }
            }
        } else if let Some(&id) = copy.ids.iter().find(|&&id| self.object_by_id(id).is_some()) {
            bail!("an object with id {id} is already in this file");
        }
        let mut splices = Vec::new();
        let mut counts = HashMap::new();
        self.insert_into(
            group,
            index,
            widget,
            group_bytes,
            &[],
            &mut splices,
            &mut counts,
        )?;
        self.finish(splices, counts, &copy.classes)
    }

    // Splices for putting a widget (and its GroupTemplate) into `group`.
    // Slots are positions in the file before any removal; elements in `skip`
    // (a widget being moved) do not count, so `index` is relative to the
    // remaining children.
    #[allow(clippy::too_many_arguments)]
    fn insert_into(
        &self,
        group: usize,
        index: usize,
        widget: Vec<u8>,
        group_bytes: Option<Vec<u8>>,
        skip: &[usize],
        splices: &mut Vec<Splice>,
        counts: &mut HashMap<usize, i64>,
    ) -> Result<()> {
        let (count, mut elements) = self
            .array(group, Prop::WIDGET_INSTANCES)
            .context("the group stores no WidgetInstances array")?;
        elements.retain(|e| !skip.contains(e));
        let at = self.array_slot(count, &elements, index);
        splices.push(Splice {
            range: at..at,
            bytes: widget,
        });
        *counts.entry(count).or_default() += 1;
        if let Some(bytes) = group_bytes {
            let (count, mut elements) = self
                .array(group, Prop::GROUPS)
                .context("the group stores no Groups array")?;
            elements.retain(|e| !skip.contains(e));
            let at = self.array_slot(count, &elements, usize::MAX);
            splices.push(Splice {
                range: at..at,
                bytes,
            });
            *counts.entry(count).or_default() += 1;
        }
        Ok(())
    }

    // Splices for taking widget `w` out: its object and its GroupTemplate.
    // Returns the removed object spans.
    fn take_out(
        &self,
        widgets: &[Widget],
        w: usize,
        splices: &mut Vec<Splice>,
        counts: &mut HashMap<usize, i64>,
    ) -> Result<Vec<usize>> {
        ensure!(w > 0 && w < widgets.len(), "the root cannot be removed");
        let mut roots = vec![widgets[w].object];
        if widgets[w].class == Class::GROUP_INSTANCE {
            roots.extend(self.child_group(widgets, w));
        }
        for &o in &roots {
            let f = self.spans[o].field.context("widget outside any array")?;
            let count = (f.object..o)
                .rev()
                .find(|&i| {
                    self.spans[i].field == Some(f)
                        && matches!(self.spans[i].record, Record::Count(_))
                })
                .context("array count not found")?;
            *counts.entry(count).or_default() -= 1;
            splices.push(Splice {
                range: self.subtree_bytes(o),
                bytes: Vec::new(),
            });
        }
        Ok(roots)
    }

    // Applies the count changes as splices, then rebuilds.
    pub(super) fn finish(
        &self,
        mut splices: Vec<Splice>,
        counts: HashMap<usize, i64>,
        classes: &[u32],
    ) -> Result<Asset> {
        for (span, delta) in counts {
            let Record::Count(n) = self.spans[span].record else {
                unreachable!("count span")
            };
            let n = i64::from(n) + delta;
            ensure!(n >= 0, "array count would drop below zero");
            let mut bytes = Vec::new();
            crate::varint::write(&mut bytes, n as u32);
            splices.push(Splice {
                range: self.spans[span].range.clone(),
                bytes,
            });
        }
        self.rebuild(&splices, classes)
    }

    /// Removes widget `w` and everything under it.
    pub fn remove_widget(&self, widgets: &[Widget], w: usize) -> Result<Removed> {
        let mut splices = Vec::new();
        let mut counts = HashMap::new();
        let roots = self.take_out(widgets, w, &mut splices, &mut counts)?;
        let gone: Vec<Range<usize>> = roots.iter().map(|&o| o..self.subtree_end(o)).collect();
        let ids: Vec<Guid> = gone
            .iter()
            .flat_map(|r| r.clone())
            .filter_map(|i| match &self.spans[i].record {
                Record::Object(o) => match o.id {
                    ObjectId::Id(g) => Some(g),
                    _ => None,
                },
                _ => None,
            })
            .collect();
        let dangling = self
            .references()
            .filter(|(i, r)| ids.contains(&r.id) && !gone.iter().any(|g| g.contains(i)))
            .count();
        Ok(Removed {
            asset: self.finish(splices, counts, &[])?,
            dangling,
        })
    }

    /// Moves widget `w` to child `index` of widget `parent`, counted among
    /// the parent's children without `w`. Ids stay, so references follow.
    pub fn move_widget(
        &self,
        widgets: &[Widget],
        w: usize,
        parent: usize,
        index: usize,
    ) -> Result<Asset> {
        let group = self
            .child_group(widgets, parent)
            .context("only the root and groups can hold widgets")?;
        let mut splices = Vec::new();
        let mut counts = HashMap::new();
        let roots = self.take_out(widgets, w, &mut splices, &mut counts)?;
        if let Some(&own) = roots.get(1) {
            ensure!(
                group != own && !(own..self.subtree_end(own)).contains(&group),
                "a group cannot move into itself"
            );
        }
        let copy = self.widget_copy(widgets, w)?;
        self.insert_into(
            group,
            index,
            copy.widget,
            copy.group,
            &roots,
            &mut splices,
            &mut counts,
        )?;
        self.finish(splices, counts, &[])
    }
}

fn replace_all(bytes: &mut [u8], from: &[u8; 16], to: &[u8; 16]) {
    let mut i = 0;
    while i + 16 <= bytes.len() {
        if &bytes[i..i + 16] == from {
            bytes[i..i + 16].copy_from_slice(to);
            i += 16;
        } else {
            i += 1;
        }
    }
}

// ---------------------------------------------------------------------------
// Colour palettes
//
// ColorTypes name a palette entry (`ColorPaletteAsset` reference + entry
// id) besides their embedded RGB; the palette's colour is what the game
// shows. 9,696 of the install's 26,634 ColorTypes do, all into the one
// shared palette `c63efd8b…` (124 named colours: Anno117_Base_Blue,
// Anno117_TextIcon_Light…). 1800 ships a palette under the same guid with
// colours of its own, so palettes belong to an install: an asset resolves
// against the set its loader gave it, and without one shows the embedded
// RGB.

type Palette = HashMap<u32, [u8; 3]>;

/// One install's colour palettes (`ColorPaletteAsset`), which ColorTypes
/// naming an entry resolve through once an asset uses them
/// ([`Asset::use_palettes`]).
#[derive(Default)]
pub struct Palettes {
    by_guid: std::sync::RwLock<HashMap<Guid, std::sync::Arc<Palette>>>,
    /// Bumped by every palette registered: widgets read before resolved
    /// against fewer.
    generation: std::sync::atomic::AtomicU64,
}

impl Palettes {
    /// Registers `asset` if it is a ColorPaletteAsset (`PHXPA`), so
    /// ColorTypes naming its entries resolve to its colours; returns how
    /// many colours it has. Other assets are ignored (0).
    pub fn register(&self, asset: &Asset) -> usize {
        let Some(guid) = asset.guid() else { return 0 };
        let colors = palette_colors(asset);
        let n = colors.len();
        if n > 0
            && let Ok(mut p) = self.by_guid.write()
        {
            p.insert(guid, std::sync::Arc::new(colors));
            self.generation
                .fetch_add(1, std::sync::atomic::Ordering::Release);
        }
        n
    }

    pub(super) fn color(&self, palette: Guid, entry: u32) -> Option<[u8; 3]> {
        self.by_guid
            .read()
            .ok()?
            .get(&palette)?
            .get(&entry)
            .copied()
    }
}

impl Asset {
    /// Resolves this asset's ColorTypes through `palettes`, its install's,
    /// from now on.
    pub fn use_palettes(&self, palettes: &std::sync::Arc<Palettes>) {
        let mut slot = self.widgets.palettes.write().expect("palettes lock");
        if !slot
            .as_ref()
            .is_some_and(|p| std::sync::Arc::ptr_eq(p, palettes))
        {
            *slot = Some(palettes.clone());
        }
    }

    pub(super) fn palette_color(&self, palette: Guid, entry: u32) -> Option<[u8; 3]> {
        self.widgets
            .palettes
            .read()
            .ok()?
            .as_ref()?
            .color(palette, entry)
    }

    /// Which palettes, at which generation, the widgets would be read with.
    fn palette_key(&self) -> super::PaletteKey {
        match &*self.widgets.palettes.read().expect("palettes lock") {
            Some(p) => (
                std::sync::Arc::as_ptr(p) as usize,
                p.generation.load(std::sync::atomic::Ordering::Acquire),
            ),
            None => (0, 0),
        }
    }
}

/// A ColorPaletteAsset's colours by entry id; empty for other assets.
fn palette_colors(asset: &Asset) -> Palette {
    let Some((root, _)) = asset.objects().next() else {
        return Palette::new();
    };
    let mut colors = Palette::new();
    for c in asset.values_of(root, Prop::PALETTE_COLORS) {
        if !matches!(&asset.spans[c].record, Record::Object(h) if h.class == Class::PALETTE_COLOR) {
            continue;
        }
        let mut rgb = None;
        let mut entry = None;
        for i in c + 1..asset.spans.len() {
            match (&asset.spans[i].record, asset.spans[i].field) {
                (_, Some(f)) if f.object < c => break,
                (Record::Color(v), _) => rgb = Some(*v),
                (Record::Value(Value::U32(v)), Some(f))
                    if f.object == c && asset.prop_crc(f) == Some(Prop::PALETTE_ENTRY) =>
                {
                    entry = Some(*v)
                }
                _ => {}
            }
        }
        if let (Some(e), Some(rgb)) = (entry, rgb) {
            colors.insert(e, rgb);
        }
    }
    colors
}

/// Decodes an ImageAsset (`PHXIM`): texture path, size, 9-slice rect, atlas.
pub fn image_info(asset: &Asset) -> Option<ImageInfo> {
    let (root, header) = asset.objects().next()?;
    if header.class != Class::IMAGE_ASSET || !asset.typed {
        return None;
    }
    let fields = asset.fields();
    let mut info = ImageInfo {
        path: String::new(),
        size: [0.0; 2],
        slice: [[0.0; 2], [1.0; 2]],
        filtering: 0,
        atlas: None,
    };
    for p in asset.props_with(root, &fields) {
        match (p.crc, p.value) {
            (Prop::IMAGE_ID, Value::Str(s)) => info.path = s,
            (Prop::SIZE, Value::Vec2(v)) => info.size = v,
            (Prop::TEXTURE_FILTERING, Value::Enum { value, .. }) => info.filtering = value,
            (Prop::IMAGE_ATLAS, Value::Ref(r)) => info.atlas = Some(r.id),
            (Prop::UV_RECT, _) => {
                for q in asset.props_with(p.span, &fields) {
                    match (q.crc, q.value) {
                        (Prop::TOP_LEFT, Value::Vec2(v)) => info.slice[0] = v,
                        (Prop::BOTTOM_RIGHT, Value::Vec2(v)) => info.slice[1] = v,
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
    Some(info)
}

/// A TextStyle asset (`PHXTS`, root class `0x9f98c15a`): what labels that
/// reference it draw with. Alignment lives on the label.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct TextStyle {
    /// LocalizedFontAsset (`PHXLF`).
    pub font: Option<Reference>,
    /// Font size in reference pixels (`Size`).
    pub size: f32,
    /// `BaseColor` RGB.
    pub color: Option<[u8; 3]>,
    /// Outline colour, when `OutlineEnabled`.
    outline: Option<[u8; 3]>,
    outline_alpha: f32,
    /// Drop shadow colour when `ShadowEnabled`; alpha, distance (px),
    /// angle (degrees).
    pub shadow: Option<[u8; 3]>,
    pub shadow_alpha: f32,
    pub shadow_distance: f32,
    pub shadow_angle: f32,
    pub letter_spacing: f32,
}

/// The FontAsset (`PHXFT`) a LocalizedFontAsset (`PHXLF`) uses for the
/// first language it lists (English in every shipped one).
pub fn localized_font(asset: &Asset) -> Option<Guid> {
    asset
        .references()
        .find(|(_, r)| r.class == Class::FONT_ASSET)
        .map(|(_, r)| r.id)
}

/// The font file a FontAsset (`PHXFT`) loads, as stored
/// (`data\fonts\SalfordSans-Medium.otf`).
pub fn font_file(asset: &Asset) -> Option<String> {
    asset.strings().map(|(_, s)| s.text.clone()).find(|t| {
        let l = t.to_ascii_lowercase();
        [".otf", ".ttf", ".ttc"].iter().any(|e| l.ends_with(e))
    })
}

/// Decodes a TextStyle asset.
pub fn text_style(asset: &Asset) -> Option<TextStyle> {
    let (root, header) = asset.objects().next()?;
    if header.class != Class::TEXT_STYLE || !asset.typed {
        return None;
    }
    let mut t = TextStyle::default();
    let (mut outline, mut shadow) = (false, false);
    let (mut outline_rgb, mut shadow_rgb) = (None, None);
    for p in asset.object_props(root) {
        match (p.crc, p.value) {
            (Prop::LOCALIZED_FONT, Value::Ref(r)) => t.font = Some(r),
            (Prop::SIZE, Value::F32(v)) => t.size = v,
            (Prop::BASE_COLOR, Value::Color(c)) => t.color = Some(c),
            (Prop::OUTLINE_ENABLED, Value::Bool(b)) => outline = b,
            (Prop::OUTLINE_COLOR, Value::Color(c)) => outline_rgb = Some(c),
            (Prop::OUTLINE_ALPHA, Value::F32(v)) => t.outline_alpha = v,
            (Prop::SHADOW_ENABLED, Value::Bool(b)) => shadow = b,
            (Prop::SHADOW_COLOR, Value::Color(c)) => shadow_rgb = Some(c),
            (Prop::SHADOW_ALPHA, Value::F32(v)) => t.shadow_alpha = v,
            (Prop::SHADOW_DISTANCE, Value::F32(v)) => t.shadow_distance = v,
            (Prop::SHADOW_ANGLE, Value::F32(v)) => t.shadow_angle = v,
            (Prop::LETTER_SPACING, Value::F32(v)) => t.letter_spacing = v,
            _ => {}
        }
    }
    t.outline = outline_rgb.filter(|_| outline);
    t.shadow = shadow_rgb.filter(|_| shadow);
    Some(t)
}

/// The text-table line id of a label `LocalizationId` such as
/// `"A01A 9EEC 1DD0 3236"`: the 16 hex digits are the line id's 64-bit two's
/// complement, big-endian (`<LineId>-6910035941296950730</LineId>`).
pub fn localization_line_id(id: &str) -> Option<i64> {
    let hex: String = id.chars().filter(|c| !c.is_whitespace()).collect();
    if hex.len() != 16 {
        return None;
    }
    u64::from_str_radix(&hex, 16).ok().map(|v| v as i64)
}

/// Line id -> text from `data/base/config/gui/texts_<language>.xml`
/// (`<Text><LineId>…</LineId><Text>…</Text></Text>`, ~35k lines in English).
pub fn parse_texts(xml: &str) -> HashMap<i64, String> {
    let mut out = HashMap::new();
    let (start, end) = if xml.contains("<LineId>") {
        ("<LineId>", "</LineId>")
    } else {
        ("<GUID>", "</GUID>")
    };
    let mut rest = xml;
    while let Some(i) = rest.find(start) {
        rest = &rest[i + start.len()..];
        let Some(j) = rest.find(end) else {
            break;
        };
        let id = rest[..j].trim().parse::<i64>();
        rest = &rest[j..];
        // The line's text is the next <Text> element before the next line.
        let line = &rest[..rest.find(start).unwrap_or(rest.len())];
        const OPEN: &str = "<Text>";
        if let (Ok(id), Some(a)) = (id, line.find(OPEN))
            && let Some(b) = line[a + OPEN.len()..].find("</Text>")
        {
            let text = a + OPEN.len();
            out.insert(id, unescape_xml(&line[text..text + b]));
        }
    }
    out
}

fn unescape_xml(s: &str) -> String {
    if !s.contains('&') {
        return s.to_owned();
    }
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

// ---------------------------------------------------------------------------
// Components

/// One ComponentInstance (or EffectInstance) on a widget.
#[derive(Debug, Clone, PartialEq)]
pub struct Component {
    /// Object header span of the ComponentInstance.
    object: usize,
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
            if o.class != Class::PROPERTY_VALUE
                || !self.owned_by(i, widget.object, Prop::PROPERTY_VALUES)
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
                    (Some(Prop::PROPERTY_ID_PATH), Record::Value(Value::U32(v))) => path.push(*v),
                    (Some(Prop::VALUE), Record::Default(_)) if value.is_none() => {
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
            Class::COMPONENT_INSTANCE,
            Prop::COMPONENTS,
            Prop::COMPONENT_TEMPLATE,
        )
    }

    /// The EffectInstances on a widget (`Effects`), shaped like components:
    /// `template` is the EffectTemplate (`PHXEF`), overrides address its
    /// custom properties the same way (`[effect uid, property uid]`).
    pub fn effects(&self, widget: &Widget) -> Vec<Component> {
        self.instances(
            widget,
            Class::EFFECT_INSTANCE,
            Prop::EFFECTS,
            Prop::EFFECT_TEMPLATE,
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
                    (Prop::UNIQUE_ID, Value::U32(v)) => c.uid = v,
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
