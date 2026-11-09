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

use super::{Asset, Field, Guid, ObjectId, Record, Reference, Splice, Value, is_a, name_crc};

const CLASS_GROUP_TEMPLATE: u32 = name_crc(b"GroupTemplate");
const CLASS_GROUP_INSTANCE: u32 = name_crc(b"GroupInstance");
const CLASS_WIDGET_INSTANCE: u32 = name_crc(b"WidgetInstance");
const CLASS_IMAGE_ASSET: u32 = name_crc(b"ImageAsset");
const CLASS_ATLAS_DESCRIPTION: u32 = name_crc(b"AtlasDescription");

/// One property of an object as the editor shows it.
#[derive(Debug, Clone, PartialEq)]
pub struct PropValue {
    pub name: Option<&'static str>,
    /// crc32 of the property name.
    pub crc: u32,
    /// Wire property index (presence-mask bit).
    pub prop: u16,
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
    pub filtering: u32,
    /// The ImageAtlasAsset this image is a region of, if any.
    pub atlas: Option<Guid>,
}

// Property name crcs.
const PROP_POSITION: u32 = name_crc(b"Position");
const PROP_PIVOT: u32 = name_crc(b"PivotPoint");
const PROP_ROTATION: u32 = name_crc(b"RotationAngle");
const PROP_SCALE: u32 = name_crc(b"Scale");
const PROP_ALPHA_TINT: u32 = name_crc(b"AlphaTint");
const PROP_COLOR_TINT: u32 = name_crc(b"ColorTint");
const PROP_VISIBLE: u32 = name_crc(b"Visible");
const PROP_IS_HIDDEN: u32 = name_crc(b"IsHidden");
const PROP_ANCHOR: u32 = name_crc(b"AnchorPoint");
// Anchor metrics x/y (unnamed; registered as legacy aliases),
// then WidthResizing/HeightResizing.
const PROP_ANCHOR_METRICS: [u32; 2] = [0x5c63_f4bb, 0xc16c_15cd];
const PROP_RESIZE_METRICS: [u32; 2] = [name_crc(b"WidthResizing"), name_crc(b"HeightResizing")];
const PROP_ASPECT_RATIO: u32 = name_crc(b"AspectRatio");
const PROP_Z_INDEX: u32 = name_crc(b"ZIndex");
pub(super) const PROP_ENUM_ENTRY: u32 = name_crc(b"EnumEntry");
const PROP_WIDGET_TYPE: u32 = name_crc(b"WidgetType");
const PROP_PARENT_WIDGET_TYPE: u32 = name_crc(b"ParentWidgetType");
const PROP_CURRENT_ENUM_VALUE: u32 = name_crc(b"CurrentEnumValue");
const PROP_CONSIDER_SIZE: u32 = name_crc(b"ConsiderSize");
const PROP_WIDGET_INSTANCES: u32 = name_crc(b"WidgetInstances");
const PROP_GROUP_TEMPLATE: u32 = name_crc(b"GroupTemplate");
const PROP_NAME: u32 = super::PROP_NAME;
const PROP_IMAGE_ID: u32 = name_crc(b"ImageId");
const PROP_SIZE: u32 = name_crc(b"Size");
// Unnamed ImageAsset field holding the UV rectangle object.
const PROP_UV_RECT: u32 = 0x65fc_c08e;
const PROP_TEXTURE_FILTERING: u32 = name_crc(b"TextureFiltering");
const PROP_IMAGE_ATLAS: u32 = name_crc(b"ImageAtlas");
const PROP_TOP_LEFT: u32 = name_crc(b"TopLeft");
const PROP_BOTTOM_RIGHT: u32 = name_crc(b"BottomRight");
const PROP_RESOLUTION: u32 = name_crc(b"Resolution");
const PROP_IMAGE: u32 = name_crc(b"Image");
const PROP_UV_OFFSET: u32 = name_crc(b"UVOffset");
const PROP_UV_SCALE: u32 = name_crc(b"UVScale");
const PROP_UV_ROTATION: u32 = name_crc(b"UVRotation");
const PROP_FILL_MODE: u32 = name_crc(b"FillMode");
/// See [`WidgetImage::path`]; its name is unknown.
const PROP_IMAGE_PATH: u32 = name_crc(b"DynamicImageId");
const PROP_TEXT: u32 = name_crc(b"Text");
const PROP_DISPLAYED_TEXT: u32 = name_crc(b"DisplayedText");
const PROP_LOCALIZATION_ID: u32 = name_crc(b"LocalizationId");
// Unnamed; self-typed with the PHXLG kind root class.
const PROP_FONT: u32 = 0xe152_3167;
const PROP_TEXT_STYLE: u32 = name_crc(b"TextStyle");
const PROP_ALIGNMENT: [u32; 2] = [name_crc(b"AlignmentX"), name_crc(b"AlignmentY")];
const PROP_TEXT_PLANE: u32 = name_crc(b"TextPlane");
const PROP_OVERFLOW: [u32; 2] = [
    name_crc(b"HorizontalOverflow"),
    name_crc(b"VerticalOverflow"),
];
const PROP_SIZING: [u32; 2] = [name_crc(b"HorizontalSizing"), name_crc(b"VerticalSizing")];
const PROP_MAX_LINES: u32 = name_crc(b"MaxLineCount");
const PROP_LINE_SPACING: u32 = name_crc(b"LineSpacing");
const CLASS_IMAGE_DESCRIPTION: u32 = name_crc(b"ImageDescription");
// TextStyle root; the PHXTS kind root 0xc697c96c is its abstract base.
const CLASS_TEXT_STYLE: u32 = name_crc(b"SlugTextStyleAsset");
const PROP_TRANSLATION: u32 = name_crc(b"Translation");
const PROP_ATLAS_ROTATION: u32 = name_crc(b"Rotation");
const PROP_LOCALIZED_FONT: u32 = name_crc(b"LocalizedFont");
const PROP_BASE_COLOR: u32 = name_crc(b"BaseColor");
const PROP_OUTLINE_ENABLED: u32 = name_crc(b"OutlineEnabled");
// Unnamed ColorType right after OutlineEnabled.
const PROP_OUTLINE_COLOR: u32 = name_crc(b"OutlineColor");
const PROP_OUTLINE_ALPHA: u32 = name_crc(b"OutlineAlpha");
const PROP_SHADOW_ENABLED: u32 = name_crc(b"ShadowEnabled");
const PROP_SHADOW_COLOR: u32 = name_crc(b"ShadowColor");
const PROP_SHADOW_ALPHA: u32 = name_crc(b"ShadowAlpha");
const PROP_SHADOW_DISTANCE: u32 = name_crc(b"ShadowDistance");
const PROP_SHADOW_ANGLE: u32 = name_crc(b"ShadowAngle");
const PROP_LETTER_SPACING: u32 = name_crc(b"LetterSpacing");
const CLASS_CONTAINER_TEMPLATE: u32 = name_crc(b"ContainerWidgetTemplate");
const CLASS_CONTAINER_INSTANCE: u32 = name_crc(b"ContainerWidgetInstance");
const PROP_REFERENCE_SIZE: u32 = name_crc(b"ReferenceSize");
const PROP_TEMPLATE: u32 = name_crc(b"Template");
const CLASS_IMAGE_WIDGET: u32 = name_crc(b"ImageWidgetInstance");
const CLASS_LABEL_WIDGET: u32 = name_crc(b"LabelWidgetInstance");

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
            Record::Object(o) if o.class == super::CLASS_COLOR => fields
                .get(&Field {
                    object: span,
                    prop: 1,
                })
                .map_or(Value::Other, |c| self.span_value(c, fields)),
            // A ColorType: its palette entry when that palette is known,
            // else the RGB of its embedded Color.
            Record::Object(o) if o.class == super::CLASS_COLOR_TYPE => {
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
                    && let Some(rgb) = palette_color(g, e)
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
    /// are not widget templates or were not fully typed. Read once per
    /// palette registered (ColorType values resolve through the palettes):
    /// layout asks for it per template and per container instance.
    pub fn widgets(&self) -> std::sync::Arc<[Widget]> {
        let generation = palette_generation();
        let mut cached = self.widgets.0.lock().expect("widget cache");
        if let Some((g, w)) = &*cached
            && *g == generation
        {
            return w.clone();
        }
        let w: std::sync::Arc<[Widget]> = self.read_widgets().into();
        *cached = Some((generation, w.clone()));
        w
    }

    fn read_widgets(&self) -> Vec<Widget> {
        let Some((root, root_header)) = self.objects().next() else {
            return Vec::new();
        };
        if !self.typed || !is_a(root_header.class, CLASS_GROUP_TEMPLATE) {
            return Vec::new();
        }
        let fields = self.fields();
        let name_of = |o: usize| {
            self.props_with(o, &fields)
                .into_iter()
                .find(|p| p.crc == PROP_NAME)
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
                && is_a(o.class, CLASS_GROUP_TEMPLATE)
            {
                group_by_id.insert(id, i);
            }
            if !is_a(o.class, CLASS_WIDGET_INSTANCE) {
                continue;
            }
            if let Some(f) = self.spans[i].field
                && self.prop_crc(f) == Some(PROP_WIDGET_INSTANCES)
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
                if h.class == CLASS_GROUP_INSTANCE
                    && let Some(Value::Ref(r)) = props
                        .iter()
                        .find(|p| p.crc == PROP_GROUP_TEMPLATE)
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

    /// Layout fields of a WidgetInstance; None for the root and non-widgets.
    /// A container instance's stored `CurrentEnumValue`: the style of its
    /// template it starts in (0 when absent).
    pub fn current_enum_value(&self, widget: &Widget) -> u32 {
        widget
            .props
            .iter()
            .find_map(|p| match (p.crc, &p.value) {
                (PROP_CURRENT_ENUM_VALUE, Value::U32(v)) => Some(*v),
                _ => None,
            })
            .unwrap_or(0)
    }

    pub fn geometry(&self, widget: &Widget) -> Option<Geometry> {
        if !is_a(widget.class, CLASS_WIDGET_INSTANCE) {
            return None;
        }
        let mut g = Geometry::default();
        for p in &widget.props {
            match (p.crc, &p.value) {
                (PROP_POSITION, Value::Vec3(v)) => g.position = *v,
                (PROP_PIVOT, Value::Vec3(v)) => g.pivot = *v,
                (PROP_ROTATION, Value::F32(v)) => g.rotation = *v,
                (PROP_SCALE, Value::Vec3(v)) => g.scale = *v,
                (PROP_ALPHA_TINT, Value::F32(v)) => g.alpha = *v,
                (PROP_COLOR_TINT, Value::Color(c)) => g.tint = Some(*c),
                (PROP_VISIBLE, Value::Bool(v)) => g.visible = *v,
                (PROP_IS_HIDDEN, Value::Bool(v)) => g.hidden = *v,
                (PROP_ANCHOR, Value::Enum { value, .. }) => g.anchor = *value,
                (PROP_Z_INDEX, Value::U32(v)) => g.z_index = *v,
                (PROP_ENUM_ENTRY, Value::U32(v)) => g.enum_entry = *v,
                (PROP_CONSIDER_SIZE, Value::Bool(v)) => g.consider_size = *v,
                (PROP_ASPECT_RATIO, Value::Enum { value, .. }) => g.aspect_ratio = *value,
                (c, Value::Enum { value, .. }) if PROP_ANCHOR_METRICS.contains(&c) => {
                    g.anchor_metrics[usize::from(c == PROP_ANCHOR_METRICS[1])] = *value;
                }
                (c, Value::Enum { value, .. }) if PROP_RESIZE_METRICS.contains(&c) => {
                    g.resize_metrics[usize::from(c == PROP_RESIZE_METRICS[1])] = *value;
                }
                _ => {}
            }
        }
        Some(g)
    }

    /// The root template's `ReferenceSize`: the pixel size its widgets are
    /// laid out in (1920 × 1080 for screens and most templates).
    pub fn reference_size(&self) -> Option<[f32; 2]> {
        let (root, h) = self.objects().next()?;
        if !is_a(h.class, CLASS_CONTAINER_TEMPLATE) {
            return None;
        }
        self.object_props(root).into_iter().find_map(|p| match p {
            PropValue {
                crc: PROP_REFERENCE_SIZE,
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
                crc: PROP_WIDGET_TYPE | PROP_PARENT_WIDGET_TYPE,
                value: Value::Ref(r),
                ..
            } => Some(r),
            _ => None,
        })
    }

    /// The ContainerWidgetTemplate asset a ContainerWidgetInstance expands
    /// to (`data/ui/studio/generated/<guid>`); its widgets are this widget's
    /// children.
    pub fn container_template(&self, widget: &Widget) -> Option<Reference> {
        if !is_a(widget.class, CLASS_CONTAINER_INSTANCE) {
            return None;
        }
        widget.props.iter().find_map(|p| match p {
            PropValue {
                crc: PROP_TEMPLATE,
                value: Value::Ref(r),
                ..
            } => Some(*r),
            _ => None,
        })
    }

    /// The image of an ImageWidgetInstance.
    pub fn widget_image(&self, widget: &Widget) -> Option<WidgetImage> {
        if !is_a(widget.class, CLASS_IMAGE_WIDGET) {
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
        for p in &widget.props {
            match (p.crc, &p.value) {
                (PROP_IMAGE, Value::Ref(r)) => img.asset = Some(*r),
                (PROP_FILL_MODE, Value::Enum { value, .. }) => img.fill = *value,
                (PROP_UV_OFFSET, Value::Vec2(v)) => img.uv_offset = *v,
                (PROP_UV_SCALE, Value::Vec2(v)) => img.uv_scale = *v,
                (PROP_UV_ROTATION, Value::F32(v)) => img.uv_rotation = *v,
                (PROP_IMAGE_PATH, Value::Str(s)) if !s.is_empty() => img.path = Some(s.clone()),
                _ => {}
            }
        }
        Some(img)
    }

    /// The text of a LabelWidgetInstance.
    pub fn label(&self, widget: &Widget) -> Option<LabelText> {
        if !is_a(widget.class, CLASS_LABEL_WIDGET) {
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
        for p in &widget.props {
            match (p.crc, &p.value) {
                (PROP_TEXT, Value::Str(s)) => l.text = s.clone(),
                (PROP_DISPLAYED_TEXT, Value::Str(s)) => l.displayed_text = s.clone(),
                (PROP_LOCALIZATION_ID, Value::Str(s)) => l.localization_id = s.clone(),
                (PROP_FONT, Value::Ref(r)) => l.font = Some(*r),
                (PROP_TEXT_STYLE, Value::Ref(r)) => l.style = Some(*r),
                (c, Value::U32(v)) if PROP_ALIGNMENT.contains(&c) => {
                    l.alignment[usize::from(c == PROP_ALIGNMENT[1])] = *v;
                }
                (c, Value::U32(v)) if PROP_OVERFLOW.contains(&c) => {
                    l.overflow[usize::from(c == PROP_OVERFLOW[1])] = *v;
                }
                (c, Value::U32(v)) if PROP_SIZING.contains(&c) => {
                    l.sizing[usize::from(c == PROP_SIZING[1])] = *v;
                }
                (PROP_TEXT_PLANE, Value::Vec2(v)) => l.text_plane = *v,
                (PROP_MAX_LINES, Value::F32(v)) => l.max_lines = *v,
                (PROP_LINE_SPACING, Value::F32(v)) => l.line_spacing = *v,
                _ => {}
            }
        }
        Some(l)
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
// `Asset::rebuild` re-derives the class table and layout section.

const PROP_GROUPS: u32 = name_crc(b"Groups");

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
        if widget.class != CLASS_GROUP_INSTANCE {
            return None;
        }
        widget.props.iter().find_map(|p| match (p.crc, &p.value) {
            (PROP_GROUP_TEMPLATE, Value::Ref(r)) => self.object_by_id(r.id),
            _ => None,
        })
    }

    /// Copies widget `w` (index into `widgets`) with everything under it.
    pub fn widget_copy(&self, widgets: &[Widget], w: usize) -> Result<WidgetCopy> {
        ensure!(w > 0 && w < widgets.len(), "the root cannot be copied");
        let object = widgets[w].object;
        let group = if widgets[w].class == CLASS_GROUP_INSTANCE {
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

    /// Inserts several copies as the last children of widget `parent`, in
    /// order, with one rebuild: the whole tree is emitted in a single encode
    /// instead of one full decode-encode round per widget. With `fresh`,
    /// every object id inside each copy gets a new random id (and references
    /// inside the copy follow), so the same copy can be inserted next to its
    /// original and copies sharing ids do not collide with each other.
    pub fn insert_widgets_batch(
        &self,
        widgets: &[Widget],
        parent: usize,
        copies: &[WidgetCopy],
        fresh: bool,
    ) -> Result<Asset> {
        if copies.is_empty() {
            bail!("no widgets to insert");
        }
        let group = self
            .child_group(widgets, parent)
            .context("only the root and groups can hold widgets")?;
        let mut splices = Vec::new();
        let mut counts = HashMap::new();
        let mut classes = Vec::new();
        for copy in copies {
            for c in &copy.classes {
                if !classes.contains(c) {
                    classes.push(*c);
                }
            }
        }
        if !fresh {
            let mut flat = Vec::new();
            for copy in copies {
                for id in &copy.ids {
                    if self.object_by_id(*id).is_some() {
                        bail!("an object with id {id} is already in this file");
                    }
                    if flat.contains(id) {
                        bail!("copies share object id {id}");
                    }
                    flat.push(*id);
                }
            }
        }
        for copy in copies {
            let (mut widget, mut group_bytes) = (copy.widget.clone(), copy.group.clone());
            if fresh {
                for id in &copy.ids {
                    let new = Guid::random();
                    replace_all(&mut widget, &id.0, &new.0);
                    if let Some(g) = &mut group_bytes {
                        replace_all(g, &id.0, &new.0);
                    }
                }
            }
            self.insert_into(
                group,
                usize::MAX,
                widget,
                group_bytes,
                &[],
                &mut splices,
                &mut counts,
            )?;
        }
        self.finish(splices, counts, &classes)
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
            .array(group, PROP_WIDGET_INSTANCES)
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
                .array(group, PROP_GROUPS)
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
        if widgets[w].class == CLASS_GROUP_INSTANCE {
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
            super::write_varint(&mut bytes, n as u32);
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
// Anno117_TextIcon_Light…). Palettes are global game data, so a loaded
// palette is registered once for every asset to resolve against.

const CLASS_PALETTE_COLOR: u32 = name_crc(b"ColorPaletteEntry");
const PROP_PALETTE_COLORS: u32 = name_crc(b"Colors");
const PROP_PALETTE_ENTRY: u32 = 0x46a1_7a33;

type Palette = HashMap<u32, [u8; 3]>;

fn palettes() -> &'static std::sync::RwLock<HashMap<Guid, std::sync::Arc<Palette>>> {
    static PALETTES: std::sync::OnceLock<
        std::sync::RwLock<HashMap<Guid, std::sync::Arc<Palette>>>,
    > = std::sync::OnceLock::new();
    PALETTES.get_or_init(Default::default)
}

/// Bumped by every palette registered: what was resolved against the
/// palettes before is stale.
static PALETTE_GENERATION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn palette_generation() -> u64 {
    PALETTE_GENERATION.load(std::sync::atomic::Ordering::Acquire)
}

pub(super) fn palette_color(palette: Guid, entry: u32) -> Option<[u8; 3]> {
    palettes().read().ok()?.get(&palette)?.get(&entry).copied()
}

/// Registers a ColorPaletteAsset (`PHXPA`) so ColorTypes naming its
/// entries resolve to its colours; returns how many colours it has.
/// Other assets are ignored (0).
pub fn register_palette(asset: &Asset) -> usize {
    let Some(guid) = asset.guid() else { return 0 };
    let Some((root, _)) = asset.objects().next() else {
        return 0;
    };
    let mut colors = Palette::new();
    for c in asset.values_of(root, PROP_PALETTE_COLORS) {
        if !matches!(&asset.spans[c].record, Record::Object(h) if h.class == CLASS_PALETTE_COLOR) {
            continue;
        }
        let mut rgb = None;
        let mut entry = None;
        for i in c + 1..asset.spans.len() {
            match (&asset.spans[i].record, asset.spans[i].field) {
                (_, Some(f)) if f.object < c => break,
                (Record::Color(v), _) => rgb = Some(*v),
                (Record::Value(Value::U32(v)), Some(f))
                    if f.object == c && asset.prop_crc(f) == Some(PROP_PALETTE_ENTRY) =>
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
    let n = colors.len();
    if n > 0
        && let Ok(mut p) = palettes().write()
    {
        p.insert(guid, std::sync::Arc::new(colors));
        PALETTE_GENERATION.fetch_add(1, std::sync::atomic::Ordering::Release);
    }
    n
}

/// Decodes an ImageAsset (`PHXIM`): texture path, size, 9-slice rect, atlas.
pub fn image_info(asset: &Asset) -> Option<ImageInfo> {
    let (root, header) = asset.objects().next()?;
    if header.class != CLASS_IMAGE_ASSET || !asset.typed {
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
            (PROP_IMAGE_ID, Value::Str(s)) => info.path = s,
            (PROP_SIZE, Value::Vec2(v)) => info.size = v,
            (PROP_TEXTURE_FILTERING, Value::Enum { value, .. }) => info.filtering = value,
            (PROP_IMAGE_ATLAS, Value::Ref(r)) => info.atlas = Some(r.id),
            (PROP_UV_RECT, _) => {
                for q in asset.props_with(p.span, &fields) {
                    match (q.crc, q.value) {
                        (PROP_TOP_LEFT, Value::Vec2(v)) => info.slice[0] = v,
                        (PROP_BOTTOM_RIGHT, Value::Vec2(v)) => info.slice[1] = v,
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
    Some(info)
}

/// One image packed into an atlas texture.
#[derive(Debug, Clone, PartialEq)]
pub struct AtlasRegion {
    /// The packed image's own path (`ImageId`), as ImageAssets name it.
    pub image: String,
    /// Pixel size of the region.
    pub size: [f32; 2],
    /// Pixel offset of the region in the atlas texture (`Translation`).
    pub offset: [f32; 2],
    /// `size / atlas size` (`Scale`), so the atlas size is `size / uv_scale`.
    pub uv_scale: [f32; 2],
    /// `Rotation` (0 in shipped atlases).
    pub rotation: f32,
}

/// An ImageAtlasAsset (`PHXTA`): per resolution (`Default`, …) the regions
/// it packs. An ImageAsset with an `ImageAtlas` reference finds its region
/// by matching its `ImageId` against [`AtlasRegion::image`].
pub fn atlas_regions(asset: &Asset) -> Vec<(String, Vec<AtlasRegion>)> {
    let fields = asset.fields();
    let mut out = Vec::new();
    for (i, o) in asset.objects() {
        if o.class != CLASS_ATLAS_DESCRIPTION {
            continue;
        }
        let resolution = asset
            .props_with(i, &fields)
            .into_iter()
            .find_map(|p| match (p.crc, p.value) {
                (PROP_RESOLUTION, Value::Str(s)) => Some(s),
                _ => None,
            })
            .unwrap_or_default();
        let mut regions = Vec::new();
        for (j, r) in asset.objects() {
            let owned = asset.spans[j].field.is_some_and(|f| f.object == i);
            if r.class != CLASS_IMAGE_DESCRIPTION || !owned {
                continue;
            }
            let mut region = AtlasRegion {
                image: String::new(),
                size: [0.0; 2],
                offset: [0.0; 2],
                uv_scale: [1.0; 2],
                rotation: 0.0,
            };
            for q in asset.props_with(j, &fields) {
                match (q.crc, q.value) {
                    (PROP_IMAGE_ID, Value::Str(s)) => region.image = s,
                    (PROP_SIZE, Value::Vec2(v)) => region.size = v,
                    (PROP_TRANSLATION, Value::Vec2(v)) => region.offset = v,
                    (PROP_SCALE, Value::Vec2(v)) => region.uv_scale = v,
                    (PROP_ATLAS_ROTATION, Value::F32(v)) => region.rotation = v,
                    _ => {}
                }
            }
            regions.push(region);
        }
        out.push((resolution, regions));
    }
    out
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
    pub outline: Option<[u8; 3]>,
    pub outline_alpha: f32,
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
    const CLASS_FONT_ASSET: u32 = name_crc(b"FontBaseAsset");
    asset
        .references()
        .find(|(_, r)| r.class == CLASS_FONT_ASSET)
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
    if header.class != CLASS_TEXT_STYLE || !asset.typed {
        return None;
    }
    let mut t = TextStyle::default();
    let (mut outline, mut shadow) = (false, false);
    let (mut outline_rgb, mut shadow_rgb) = (None, None);
    for p in asset.object_props(root) {
        match (p.crc, p.value) {
            (PROP_LOCALIZED_FONT, Value::Ref(r)) => t.font = Some(r),
            (PROP_SIZE, Value::F32(v)) => t.size = v,
            (PROP_BASE_COLOR, Value::Color(c)) => t.color = Some(c),
            (PROP_OUTLINE_ENABLED, Value::Bool(b)) => outline = b,
            (PROP_OUTLINE_COLOR, Value::Color(c)) => outline_rgb = Some(c),
            (PROP_OUTLINE_ALPHA, Value::F32(v)) => t.outline_alpha = v,
            (PROP_SHADOW_ENABLED, Value::Bool(b)) => shadow = b,
            (PROP_SHADOW_COLOR, Value::Color(c)) => shadow_rgb = Some(c),
            (PROP_SHADOW_ALPHA, Value::F32(v)) => t.shadow_alpha = v,
            (PROP_SHADOW_DISTANCE, Value::F32(v)) => t.shadow_distance = v,
            (PROP_SHADOW_ANGLE, Value::F32(v)) => t.shadow_angle = v,
            (PROP_LETTER_SPACING, Value::F32(v)) => t.letter_spacing = v,
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
        if let (Ok(id), Some(a)) = (id, line.find("<Text>"))
            && let Some(b) = line[a + 6..].find("</Text>")
        {
            out.insert(id, unescape_xml(&line[a + 6..a + 6 + b]));
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
