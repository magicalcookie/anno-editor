// Compiled Phoenix UI assets (`data/ui/studio/generated/<guid>`, magic
// `PHXxx`): byte-exact span decode, re-encode, and the few edits the UI editor
// offers.
//
// The file is a schema-driven object stream. The 16-byte header, the class
// table and the layout section decode fully. The body is objects
// (`02 <id form> <class crc> <prop count> <presence mask>`) followed by their
// property values in class order. The per-class property lists come from the
// exes' runtime registries (formats/data/<game>/classes.tsv, read by
// phx/tables.rs), picked per class by the schema hash in the file's class table. Assets that use a
// class or version the table lacks fall back to a scan that claims strings,
// references and colors and keeps the rest opaque.
//
// Widget trees, geometry and image lookup live in phx/widget.rs.

pub mod animation;
pub mod binding;
mod pwl;
pub use pwl::{
    EnumEntry, EnumInfo, TemplateInfo, TypeProperty, enum_entry_name, enum_info, enum_named,
    enum_of_type, property_description, register_sources, unique_entry_name, widget_type_info,
};

/// A game-object field's name by its property id (`TextData`, `Text`): the
/// exe's property rows, then its event and action rows (the scene
/// transition's ShowSingle) and Anno 1800's names for ids files kept from it
/// use (the construction bar's ConstructionBarData).
pub fn data_field_name(id: u32) -> Option<&'static str> {
    tables::data_field(id)
        .map(|f| f.1)
        .or_else(|| tables::named_id(id))
}

/// A component's or effect's custom property by UniqueId, from the shipped
/// component JSON: (component, property), `("Tween", "Started")`. For paths
/// whose component instance sits in a template that does not ship.
pub fn component_property_name(id: u32) -> Option<(&'static str, &'static str)> {
    tables::component_prop(id)
}

/// A game-object field's PropertyType code by its property id (0 bool,
/// 1 int, 3 float, 4 string…); None for objects and unknown types.
pub fn data_field_type(id: u32) -> Option<u32> {
    // Ids the stubs type one way and the screens use another (ids collide
    // across classes): unknown. Indentation (0x20e047c1) is Boolean in the
    // stubs, and the screens pass it on into Float properties 15 times.
    const UNSURE: [u32; 1] = [0x20e0_47c1];
    tables::data_field(id)
        .map(|f| f.2)
        .filter(|&t| t != u32::MAX && !UNSURE.contains(&id))
}
pub mod layout;
mod tables;
mod widget;

pub use widget::{
    AtlasRegion, Geometry, ImageInfo, LabelText, PropValue, Removed, TextStyle, Widget, WidgetCopy,
    WidgetImage, atlas_regions, font_file, image_info, localization_line_id, localized_font,
    parse_texts, register_palette, text_style,
};

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt;
use std::ops::Range;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};

use crate::game::Game;

const HEADER_LEN: usize = 0x10;

/// The CustomPropertyInstance class.
pub const CLASS_CUSTOM_PROPERTY: u32 = name_crc(b"CustomPropertyInstance");
/// Color: three RGB bytes, no alpha on the wire.
pub const CLASS_COLOR: u32 = name_crc(b"Color");
/// ColorType: optional palette reference, palette entry, `Color`.
pub const CLASS_COLOR_TYPE: u32 = name_crc(b"ColorType");
/// The EnumAsset class.
pub const CLASS_ENUM_ASSET: u32 = name_crc(b"EnumAsset");

// Color: 4 properties, 1..=3 present (R, G, B). Seen 6344 times, no variant.
const COLOR_PROPS: u8 = 4;
const COLOR_MASK: u8 = 0x70;

// Longest real string is 232 bytes. Bare (crc-less) string candidates past
// this are noise in float runs, not text.
const MAX_SCAN_STR: usize = 4096;
// Classes per file top out around 60; the varint is untrusted input.
const MAX_TYPES: u32 = 4096;
const MAX_LAYOUT_TOKENS: u32 = 1 << 22;

/// The 21 asset kinds of the runtime kind table:
/// magic letters, kind id, crc32 of the root class.
pub const KINDS: [([u8; 2], u8, u32); 21] = [
    (*b"SC", 0x00, name_crc(b"SceneTemplate")),
    (*b"WI", 0x01, name_crc(b"ContainerWidgetTemplate")),
    (*b"IM", 0x02, name_crc(b"ImageAsset")),
    (*b"VI", 0x04, name_crc(b"VideoAsset")),
    (*b"FT", 0x05, name_crc(b"FontBaseAsset")),
    (*b"TS", 0x06, name_crc(b"TextStyleBaseAsset")),
    (*b"GO", 0x07, name_crc(b"GameObjectTemplate")),
    (*b"BE", 0x0b, name_crc(b"WidgetTypeTemplate")),
    (*b"LF", 0x0c, name_crc(b"LocalizedFontAsset")),
    (*b"TA", 0x0d, name_crc(b"ImageAtlasAsset")),
    (*b"CS", 0x0e, 0x2529_e5ff),
    (*b"FD", 0x0f, name_crc(b"FontDescriptorAsset")),
    (*b"EF", 0x10, name_crc(b"EffectTemplate")),
    (*b"EN", 0x11, name_crc(b"EnumAsset")),
    (*b"CP", 0x12, name_crc(b"ComponentTemplate")),
    (*b"PA", 0x13, name_crc(b"ColorPaletteAsset")),
    (*b"LG", 0x14, 0xe152_3167),
    (*b"VS", 0x15, name_crc(b"VectorShapeAsset")),
    (*b"EC", 0x16, name_crc(b"FallbackFontGroup")),
    (*b"FF", 0x18, name_crc(b"FallbackFontAsset")),
    (*b"CO", 0x19, 0x6bf4_061d),
];

pub fn crc32(bytes: &[u8]) -> u32 {
    crc32fast::hash(bytes)
}

/// crc32 of a class or property name at compile time, so name constants
/// read as names.
pub const fn name_crc(name: &[u8]) -> u32 {
    let mut crc = !0u32;
    let mut i = 0;
    while i < name.len() {
        crc ^= name[i] as u32;
        let mut k = 0;
        while k < 8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xedb8_8320
            } else {
                crc >> 1
            };
            k += 1;
        }
        i += 1;
    }
    !crc
}

// ---------------------------------------------------------------------------
// Value types

/// A 16-byte id as stored on the wire: Microsoft mixed-endian GUID layout, so
/// the file `00214ce2-8775-…` stores its guid as `e2 4c 21 00 75 87 …`.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Guid(pub [u8; 16]);

impl Guid {
    /// A new version-4 style id. Seeded from the std hasher's per-process
    /// random keys, a clock and a counter; ids only need to be unique.
    pub fn random() -> Guid {
        use std::hash::{BuildHasher, Hasher};
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos() as u64);
        let mut b = [0u8; 16];
        for (k, chunk) in b.chunks_mut(8).enumerate() {
            let mut h = std::collections::hash_map::RandomState::new().build_hasher();
            h.write_u64(now);
            h.write_u64(n);
            h.write_usize(k);
            chunk.copy_from_slice(&h.finish().to_le_bytes());
        }
        // Mixed-endian layout: the version nibble is the high nibble of
        // byte 7, the variant the top bits of byte 8.
        b[7] = (b[7] & 0x0f) | 0x40;
        b[8] = (b[8] & 0x3f) | 0x80;
        Guid(b)
    }

    /// [`Guid::parse`] at compile time, for guid constants compared in hot
    /// paths (formatting a guid to compare it as text costs more than the
    /// lookup it guards). Panics on a malformed literal.
    pub const fn from_const(s: &str) -> Guid {
        const fn nibble(c: u8) -> u8 {
            match c {
                b'0'..=b'9' => c - b'0',
                b'a'..=b'f' => c - b'a' + 10,
                b'A'..=b'F' => c - b'A' + 10,
                _ => panic!("not a hex digit"),
            }
        }
        let s = s.as_bytes();
        let mut g = [0u8; 16];
        let (mut i, mut n) = (0, 0);
        while i < s.len() {
            if s[i] != b'-' {
                assert!(n < 32, "too many digits");
                g[n / 2] = (g[n / 2] << 4) | nibble(s[i]);
                n += 1;
            }
            i += 1;
        }
        assert!(n == 32, "a guid has 32 digits");
        // Same byte order as `parse`: the first three groups little-endian.
        let mut out = g;
        let mut k = 0;
        while k < 4 {
            out[k] = g[3 - k];
            k += 1;
        }
        out[4] = g[5];
        out[5] = g[4];
        out[6] = g[7];
        out[7] = g[6];
        Guid(out)
    }

    /// Parses the canonical `xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx` form (any
    /// case, dashes optional), e.g. an asset file name.
    pub fn parse(s: &str) -> Option<Guid> {
        let hex: Vec<u8> = s.bytes().filter(|&c| c != b'-').collect();
        if hex.len() != 32 {
            return None;
        }
        let mut g = [0u8; 16];
        for (i, pair) in hex.chunks(2).enumerate() {
            g[i] = u8::from_str_radix(std::str::from_utf8(pair).ok()?, 16).ok()?;
        }
        g[0..4].reverse();
        g[4..6].reverse();
        g[6..8].reverse();
        Some(Guid(g))
    }
}

impl fmt::Display for Guid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let b = &self.0;
        write!(
            f,
            "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-",
            b[3], b[2], b[1], b[0], b[5], b[4], b[7], b[6], b[8], b[9]
        )?;
        for x in &b[10..] {
            write!(f, "{x:02x}")?;
        }
        Ok(())
    }
}

impl fmt::Debug for Guid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Guid({self})")
    }
}

/// Bytes 0x00..0x10, kept whole. Bytes 8..16 (`09 01 ce f4 b1 ba f0 1c`) are
/// identical in all 733 shipped assets; nothing in the header depends on size.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Header(pub [u8; HEADER_LEN]);

impl Header {
    /// The two kind letters after `PHX`, e.g. `WI`.
    pub fn kind(&self) -> [u8; 2] {
        [self.0[3], self.0[4]]
    }
    pub fn kind_str(&self) -> String {
        String::from_utf8_lossy(&self.0[3..5]).into_owned()
    }
    /// Phoenix DataVersion: `1c 09 01` = 28.9.1 (also 28.7.1, 28.0.1).
    pub fn data_version(&self) -> [u8; 3] {
        [self.0[5], self.0[6], self.0[7]]
    }
    /// Kind id and root class crc from the runtime kind table, if known.
    pub fn kind_info(&self) -> Option<(u8, u32)> {
        let k = self.kind();
        KINDS.iter().find(|e| e.0 == k).map(|e| (e.1, e.2))
    }
}

/// One entry of the class table at 0x10. Entry 0 is the root class; its
/// schema hash is what the earlier notes called the per-kind "schema guid".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypeEntry {
    pub class: u32,
    /// Per-class schema hash, byte-identical to the runtime registry block.
    pub schema: [u8; 16],
    /// Heap instances of this class in the file (= its `0x40` layout tokens).
    /// Value-embedded structs such as `Color` count 0 even when present.
    pub instances: u32,
}

/// Layout section between the class table and the root object: a pre-order
/// list of the object graph's pointer slots.
///
/// Token kinds: `0x40 n class` heap object of `class` (n is always 1),
/// `0x42 n` array of n elements, `0x08 n` and `0x51 n` runs of other slots
/// (likely null and reference slots; not proven). `counts` holds the per-kind
/// sums of `n` for every kind except `0x40`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layout {
    pub counts: Vec<(u8, u32)>,
    /// Always 0 in the shipped files.
    pub reserved: u8,
    pub tokens: Vec<LayoutToken>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LayoutToken {
    pub kind: u8,
    pub count: u32,
    /// Index into the class table, for `0x40` tokens only.
    pub class_index: Option<u32>,
}

pub const TOKEN_OBJECT: u8 = 0x40;
pub const TOKEN_ARRAY: u8 = 0x42;
/// Binding index runs, see [`Asset::rebuild`].
const TOKEN_BINDING_INDICES: u8 = 0x51;
// A binding's source and target index arrays (class `0xedcf0433`).
const PROP_BINDING_SOURCES: u32 = 0x28c3_8074;
const PROP_BINDING_TARGETS: u32 = 0xcec4_084b;

/// Classes stored by value: never counted in the class table, never a
/// layout token. The same three in all 4,682 installed assets; every other
/// class is a heap object wherever it occurs.
const VALUE_CLASSES: [u32; 3] = [CLASS_COLOR_TYPE, CLASS_COLOR, 0xc761_3662];

/// Replace bytes `range` of the file (inside the body) with `bytes`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Splice {
    pub range: Range<usize>,
    pub bytes: Vec<u8>,
}

/// How an object header names itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObjectId {
    /// `02 <guid>`: the root object, carrying the file's own guid.
    Root(Guid),
    /// `02 00`: no id (value structs, most heap objects).
    None,
    /// `02 02 <id16>`: an object other records can reference (widgets, groups).
    Id(Guid),
}

/// `02 <id form> <u32 class crc> <u8 prop count> <presence mask>`.
///
/// The mask is MSB-first over the class's flattened property list; bit 0 is
/// clear in every shipped object. The doc's older "named key + tag2" reading
/// (`02 00 <crc> 05 78`) is this header for an id-less object: `05` is the
/// property count and `78` the first mask byte.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectHeader {
    pub id: ObjectId,
    pub class: u32,
    pub prop_count: u8,
    pub mask: Box<[u8]>,
    /// Located through a `0x40` layout token (heap object) rather than by scan.
    pub heap: bool,
}

impl ObjectHeader {
    pub fn present(&self, prop: usize) -> bool {
        prop < self.prop_count as usize && self.mask[prop / 8] & (0x80 >> (prop % 8)) != 0
    }
    pub fn present_count(&self) -> u32 {
        self.mask.iter().map(|b| b.count_ones()).sum()
    }
}

/// `[u32 crc32(text)] <varint len> 01 <len bytes>`. The crc prefix is the
/// class's `XxxCrc` property sitting right before `Xxx`; it is recomputed on
/// encode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StrRecord {
    pub with_crc: bool,
    pub text: String,
}

impl StrRecord {
    pub fn crc(&self) -> u32 {
        crc32(self.text.as_bytes())
    }
}

/// `01 <u32 class crc> <id16>`: a pointer to an object by id, either another
/// object in this file (`02 02 <id>`) or another asset by guid.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Reference {
    pub class: u32,
    pub id: Guid,
}

/// `CustomPropertyType_*` names for the codes confirmed against the JSON
/// templates by `UniqueId`, plus Color (its default is always a ColorType).
pub fn property_type_name(kind: u32) -> Option<&'static str> {
    Some(match kind {
        0x00 => "Boolean",
        0x01 => "Int32",
        0x02 => "UInt32",
        0x03 => "Float32",
        0x04 => "String",
        0x05 => "Vector2",
        0x06 => "Vector3",
        0x09 => "Event",
        0x0a => "Action",
        0x0b => "WidgetSibling",
        0x11 => "Enum",
        0x12 => "Color",
        0x16 => "Spacing",
        // A game object (a view model): its `object_asset` names the
        // GameObjectTemplate.
        0x13 => "Object",
        _ => return None,
    })
}

/// A CustomPropertyInstance `DefaultValue` (registry type `0x1f00`): a zero
/// byte, a type byte and its payload. The type bytes are the registry's
/// type-word high bytes (`0x08` u32, `0x10` f32, …). Floats are kept as raw
/// bits so decode/encode never alters them.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum DefaultValue {
    /// `00`: no value (events, actions, object references).
    None,
    /// `01 <u8>`.
    Bool(u8),
    /// `07 <i32>`.
    S32(i32),
    /// `08 <u32>`; enum properties store the entry value here.
    U32(u32),
    /// `10 <f32 bits>`.
    F32(u32),
    /// `11`/`12`/`13`: two, three or four f32 bits (Vector2/3/4, Spacing).
    Vec(Vec<u32>),
    /// `17 <string>`: an empty string is just `17 00`.
    Str(String),
    /// `19 <id16>`: an ObjectID (WidgetChild/WidgetSibling defaults).
    ObjectId(Guid),
    /// `40 <u32 class>`, followed by an embedded object of that class. Only
    /// ColorType is seen; its object, palette reference and Color are separate
    /// spans after this one.
    Object(u32),
}

impl DefaultValue {
    fn tag(&self) -> u8 {
        match self {
            DefaultValue::None => 0x00,
            DefaultValue::Bool(_) => 0x01,
            DefaultValue::S32(_) => 0x07,
            DefaultValue::U32(_) => 0x08,
            DefaultValue::F32(_) => 0x10,
            DefaultValue::Vec(v) => 0x0f + v.len() as u8,
            DefaultValue::Str(_) => 0x17,
            DefaultValue::ObjectId(_) => 0x19,
            DefaultValue::Object(_) => 0x40,
        }
    }

    fn encode(&self, out: &mut Vec<u8>) {
        out.push(0);
        out.push(self.tag());
        match self {
            DefaultValue::None => {}
            DefaultValue::Bool(v) => out.push(*v),
            DefaultValue::S32(v) => out.extend_from_slice(&v.to_le_bytes()),
            DefaultValue::U32(v) | DefaultValue::F32(v) | DefaultValue::Object(v) => {
                out.extend_from_slice(&v.to_le_bytes())
            }
            DefaultValue::Vec(v) => v
                .iter()
                .for_each(|x| out.extend_from_slice(&x.to_le_bytes())),
            DefaultValue::Str(s) => write_str_payload(out, s),
            DefaultValue::ObjectId(g) => out.extend_from_slice(&g.0),
        }
    }

    // The defaults the editor may rewrite: the same scalar, a vector of the
    // same length (its tag holds the length), or text (strings re-encode at
    // any length, as `Edit::Text` does).
    fn same_editable_kind(&self, other: &DefaultValue) -> bool {
        match (self, other) {
            (DefaultValue::Vec(a), DefaultValue::Vec(b)) => a.len() == b.len(),
            _ => matches!(
                (self, other),
                (DefaultValue::Bool(_), DefaultValue::Bool(_))
                    | (DefaultValue::S32(_), DefaultValue::S32(_))
                    | (DefaultValue::U32(_), DefaultValue::U32(_))
                    | (DefaultValue::F32(_), DefaultValue::F32(_))
                    | (DefaultValue::Str(_), DefaultValue::Str(_))
            ),
        }
    }
}

/// A property value. Spans hold the scalar variants ([`Record::Value`]);
/// [`PropValue`] also uses the others to describe a whole property.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Bool(bool),
    U8(u8),
    U16(u16),
    S32(i32),
    U32(u32),
    U64(u64),
    F32(f32),
    Vec2([f32; 2]),
    Vec3([f32; 3]),
    Vec4([f32; 4]),
    /// `ty` is the crc of the enum type (registry crcA); entry values are
    /// small integers.
    Enum {
        ty: u32,
        value: u32,
    },
    Guid(Guid),
    Color([u8; 3]),
    Str(String),
    Ref(Reference),
    /// A null pointer or asset reference.
    Null,
    /// Arrays and nested objects; walk the spans for those.
    Other,
}

impl Value {
    fn same_kind(&self, other: &Value) -> bool {
        match (self, other) {
            (Value::Enum { ty: a, .. }, Value::Enum { ty: b, .. }) => a == b,
            _ => std::mem::discriminant(self) == std::mem::discriminant(other),
        }
    }

    // Scalars only: the variants a Record::Value span can hold.
    fn encode(&self, out: &mut Vec<u8>) {
        let f = |out: &mut Vec<u8>, v: &[f32]| {
            v.iter()
                .for_each(|x| out.extend_from_slice(&x.to_bits().to_le_bytes()))
        };
        match self {
            Value::Bool(v) => out.push(u8::from(*v)),
            Value::U8(v) => out.push(*v),
            Value::U16(v) => out.extend_from_slice(&v.to_le_bytes()),
            Value::S32(v) => out.extend_from_slice(&v.to_le_bytes()),
            Value::U32(v) | Value::Enum { value: v, .. } => out.extend_from_slice(&v.to_le_bytes()),
            Value::U64(v) => out.extend_from_slice(&v.to_le_bytes()),
            Value::F32(v) => f(out, &[*v]),
            Value::Vec2(v) => f(out, v),
            Value::Vec3(v) => f(out, v),
            Value::Vec4(v) => f(out, v),
            Value::Guid(g) => out.extend_from_slice(&g.0),
            Value::Color(_) | Value::Str(_) | Value::Ref(_) | Value::Null | Value::Other => {
                unreachable!("not a scalar span value: {self:?}")
            }
        }
    }
}

/// What one span of the file decodes to.
#[derive(Debug, Clone, PartialEq)]
pub enum Record {
    /// Bytes 0x00..0x10, see [`Asset::header`].
    Header,
    /// The class table, see [`Asset::types`].
    Types,
    /// The layout section, see [`Asset::layout`].
    Layout,
    Object(ObjectHeader),
    /// A fixed-width scalar property value.
    Value(Value),
    Str(StrRecord),
    Ref(Reference),
    /// `00`: a null pointer or asset reference.
    Null,
    /// Varint element count of an array property.
    Count(u32),
    /// The R, G, B values of a `Color` object.
    Color([u8; 3]),
    /// A CustomPropertyInstance `DefaultValue`.
    Default(DefaultValue),
    /// The elements of a `u8` array, after its [`Record::Count`]: one span,
    /// kept verbatim. A FontDescriptor's font data is one, 99% of the bytes
    /// of all UI assets; a span per byte made each 80 bytes of memory.
    Bytes,
    /// Not understood. Kept verbatim.
    Opaque,
}

impl Record {
    fn same_kind(&self, other: &Record) -> bool {
        std::mem::discriminant(self) == std::mem::discriminant(other)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Span {
    pub range: Range<usize>,
    pub record: Record,
    /// The property this span is (part of) the value of. `None` for the
    /// header sections, the root object and spans the fallback scan claims.
    pub field: Option<Field>,
}

impl Span {
    fn new(range: Range<usize>, record: Record) -> Span {
        Span {
            range,
            record,
            field: None,
        }
    }
}

/// A property slot: the object header span and the wire property index
/// (1-based, the presence-mask bit; see [`wire_props`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Field {
    pub object: usize,
    pub prop: u16,
}

/// One class version from a game's runtime registry.
#[derive(Debug)]
pub struct Class {
    pub crc: u32,
    pub name: Option<&'static str>,
    /// Base class crc, 0 for the registry root.
    pub base: u32,
    /// Runtime instance size in bytes.
    pub size: u32,
    /// Schema hash of this version; a file's class table carries the hash
    /// of the version it was written with.
    pub hash: [u8; 16],
    /// The class's own descriptor rows, registry order.
    pub props: &'static [Prop],
}

/// An older version of a class: its own serialized props by name crc, in
/// wire order (0 = [`LEGACY_BOOL`]).
#[derive(Debug)]
pub struct Legacy {
    pub class: u32,
    pub hash: [u8; 16],
    pub props: &'static [u32],
}

/// The one old property the current classes no longer have: a bool between
/// `Font` and `ScalingFactor` in old font bindings (`0xca32a806`).
pub static LEGACY_BOOL: Prop = Prop {
    name_crc: 0,
    name: None,
    ty: 0x0100,
    type_crc: 0,
    mode: 0x2001,
    offset: 0,
};

/// One property descriptor row `{type word, value type crc, mode, name crc,
/// struct offset}`.
#[derive(Debug)]
pub struct Prop {
    pub name_crc: u32,
    pub name: Option<&'static str>,
    /// Type word: high byte = type code (`01` bool, `04` u8, `06` u16, `07`
    /// s32, `08` u32, `0a` u64, `0b` enum, `10` f32, `11`..`13` vec2..4, `17`
    /// string, `19` 16-byte id, `1f` variant, `40`/`42` pointer, `43`/`44`
    /// asset reference); low bit = array.
    pub ty: u16,
    /// Value type: enum type, pointee class or asset class crc; 0 for scalars.
    pub type_crc: u32,
    pub mode: u32,
    pub offset: u32,
}

impl Prop {
    /// Mode bit 0 (`0x2001`, `0x2009`) or bit 16 (`0x12000`) = written to the
    /// wire; `0x2000`/`0x2080` (transient) and the computed accessors
    /// (`0x1c`, `0x94`) are not.
    pub fn serialized(&self) -> bool {
        self.mode & 0x1_0001 != 0
    }
}

pub fn class_info(crc: u32) -> Option<&'static Class> {
    tables::registry(Game::Anno117)
        .class(crc)
        .or_else(|| tables::registry(Game::Anno1800).class(crc))
}

fn class_version(crc: u32, hash: &[u8; 16]) -> Option<&'static Class> {
    class_info(crc)
        .filter(|c| c.hash == *hash)
        .or_else(|| (tables::registry(Game::Anno1800).class(crc)).filter(|c| c.hash == *hash))
}

// Base of a class, including the ones the exe dropped (decoded as their base).
fn base_of(crc: u32) -> Option<u32> {
    match class_info(crc) {
        Some(c) => (c.base != 0).then_some(c.base),
        None => (tables::registry(Game::Anno117).removed.iter())
            .find(|r| r.0 == crc)
            .map(|r| r.1),
    }
}

// Own serialized props of `class` as written with schema `hash`.
fn own_props(class: u32, hash: Option<&[u8; 16]>) -> Option<Vec<&'static Prop>> {
    if let Some(version) = hash.and_then(|h| class_version(class, h)) {
        return Some(version.props.iter().filter(|p| p.serialized()).collect());
    }
    let Some(c) = class_info(class) else {
        // Registry root `Object` and dropped classes have no own props.
        let removed = &tables::registry(Game::Anno117).removed;
        return (class == CLASS_OBJECT || removed.iter().any(|r| r.0 == class)).then(Vec::new);
    };
    if hash.is_none_or(|h| *h == c.hash) {
        return Some(c.props.iter().filter(|p| p.serialized()).collect());
    }
    let legacy = (tables::registry(Game::Anno117).legacy.iter())
        .find(|l| l.class == class && Some(&l.hash) == hash)?;
    legacy
        .props
        .iter()
        .map(|&n| match n {
            0 => Some(&LEGACY_BOOL),
            n => c.props.iter().find(|p| p.name_crc == n),
        })
        .collect()
}

/// Per-file wire property lists, keyed by class: base chain first, using the
/// class version each class-table entry names. None if some class or
/// version is unknown.
fn file_schema(types: &[TypeEntry]) -> Option<HashMap<u32, Vec<&'static Prop>>> {
    let hashes: HashMap<u32, &[u8; 16]> = types.iter().map(|t| (t.class, &t.schema)).collect();
    let mut out = HashMap::new();
    for t in types {
        let mut chain = vec![t.class];
        while let Some(b) = {
            let c = *chain.last().unwrap();
            match hashes.get(&c).and_then(|h| class_version(c, h)) {
                Some(version) => (version.base != 0).then_some(version.base),
                None => base_of(c),
            }
        } {
            chain.push(b);
        }
        let mut props = Vec::new();
        for &c in chain.iter().rev() {
            props.extend(own_props(c, hashes.get(&c).copied())?);
        }
        out.insert(t.class, props);
    }
    Some(out)
}

const CLASS_OBJECT: u32 = name_crc(b"Object");

/// A class's wire property list in the current (exe) version: base chain
/// first, serialized rows only. Index `i` here is presence-mask bit `i + 1`
/// (bit 0 is never set). Assets written with older versions use
/// [`Asset::class_props`].
pub fn wire_props(crc: u32) -> Option<&'static [&'static Prop]> {
    static CACHE: std::sync::OnceLock<HashMap<u32, Vec<&'static Prop>>> =
        std::sync::OnceLock::new();
    let cache = CACHE.get_or_init(|| {
        (tables::registry(Game::Anno117).classes.iter())
            .map(|c| {
                let mut chain = vec![c];
                while let Some(b) = class_info(chain.last().unwrap().base) {
                    chain.push(b);
                }
                let props = chain
                    .iter()
                    .rev()
                    .flat_map(|c| c.props.iter().filter(|p| p.serialized()))
                    .collect();
                (c.crc, props)
            })
            .collect()
    });
    cache.get(&crc).map(|v| v.as_slice())
}

/// Whether `class` is `base` or derives from it.
pub fn is_a(class: u32, base: u32) -> bool {
    let mut c = Some(class);
    while let Some(k) = c {
        if k == base {
            return true;
        }
        c = base_of(k);
    }
    false
}

/// One decoded CustomPropertyInstance. `object`, `default` and `name` are
/// indices into [`Asset::spans`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CustomProperty {
    pub object: usize,
    pub uid: u32,
    /// `PropertyType` enum value, see [`property_type_name`].
    pub kind: u32,
    pub default: usize,
    /// Template of object-typed properties (a GameObjectTemplate).
    pub object_asset: Option<Reference>,
    /// The EnumAsset of enum-typed properties.
    pub enum_asset: Option<Reference>,
    pub name: usize,
}

// ---------------------------------------------------------------------------
// Names

/// crc32 -> name resolver: built-in names and the embedded corpus, plus an
/// optional corpus loaded at runtime. [`Asset::name`] adds each file's own
/// strings on top.
#[derive(Debug, Clone, Default)]
pub struct Names {
    map: HashMap<u32, Box<str>>,
}

// Class and property names with a verified crc. The *Widget*, Group, Track,
// ImageAtlasAsset and LocalizedFontAsset names were recovered by dictionary
// search against the class tables; each lands in a coherent family (see
// phx-decode.md section 3).
const BUILTIN_NAMES: &[&str] = &[
    "SceneTemplate",
    "ImageAsset",
    "VideoAsset",
    "GameObjectTemplate",
    "WidgetTypeTemplate",
    "FontDescriptorAsset",
    "EffectTemplate",
    "EnumAsset",
    "ComponentTemplate",
    "ColorPaletteAsset",
    "ImageAtlasAsset",
    "LocalizedFontAsset",
    "ContainerWidgetTemplate",
    "WidgetInstance",
    "ContainerWidgetInstance",
    "ImageWidgetInstance",
    "LabelWidgetInstance",
    "VideoWidgetInstance",
    "GroupTemplate",
    "GroupInstance",
    "Object",
    "Asset",
    "EnumEntry",
    "AnyEnumValue",
    "AffectedProperty",
    "CustomPropertyInstance",
    "CustomPropertyContainer",
    "ComponentInstance",
    "EffectInstance",
    "PropertyContainer",
    "PropertyValue",
    "PropertyId",
    "Color",
    "ColorType",
    "TrackKey",
    "TrackInstance",
    "FloatKey",
    "BooleanKey",
    "ColorKey",
    "EventActionKey",
    "BooleanTrackInstance",
    "ColorTrackInstance",
    "EventTrackInstance",
    "ActionTrackInstance",
    "EnumTrackInstance",
    "AnimationInstance",
    "OnPress",
    "OnRelease",
    "IsChecked",
    "BtnStates",
    "InfoTip",
    "InfoTipContext",
    "Alignment",
    "BackgroundColor",
    "ColorPalette",
];

// The harvested corpus, names only (~500 KB; the crc is recomputed on load,
// ~1 ms). Shipped binaries have no scripts/ folder, and without these names
// most UI properties show as bare crcs.
// TODO(alex): 22k Box<str> copies of static text; a sorted crc array with
// offsets into this string would need no allocation at all.
const CORPUS_NAMES: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/data/phx_names.txt"));

impl Names {
    /// The schema's class and property names, the hand-verified list, and the
    /// embedded corpus.
    pub fn builtin() -> Names {
        let mut names = Names::default();
        let schema = (tables::registry(Game::Anno117).classes.iter())
            .chain(&tables::registry(Game::Anno1800).classes)
            .flat_map(|c| {
                c.name
                    .into_iter()
                    .chain(c.props.iter().filter_map(|p| p.name))
            });
        let corpus = CORPUS_NAMES.lines().filter(|l| !l.starts_with('#'));
        for n in BUILTIN_NAMES.iter().copied().chain(schema).chain(corpus) {
            names.insert(n);
        }
        names
    }

    pub fn insert(&mut self, name: &str) -> u32 {
        let crc = crc32(name.as_bytes());
        self.map.insert(crc, name.into());
        crc
    }

    /// Merges `scripts/phx_names_corpus.json` (`{"names": {"<crc dec>": "Name"}}`).
    /// Entries whose crc does not match their name are skipped; existing names
    /// win. Returns the number of names added.
    pub fn load_corpus(&mut self, path: &Path) -> Result<usize> {
        #[derive(serde::Deserialize)]
        struct Corpus {
            names: HashMap<String, String>,
        }
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading name corpus {}", path.display()))?;
        let corpus: Corpus = serde_json::from_str(&text)
            .with_context(|| format!("parsing name corpus {}", path.display()))?;
        let mut added = 0;
        for (key, name) in corpus.names {
            let Ok(crc) = key.parse::<u32>() else {
                continue;
            };
            if crc32(name.as_bytes()) != crc {
                continue;
            }
            if let std::collections::hash_map::Entry::Vacant(e) = self.map.entry(crc) {
                e.insert(name.into());
                added += 1;
            }
        }
        Ok(added)
    }

    pub fn get(&self, crc: u32) -> Option<&str> {
        self.map.get(&crc).map(|s| &**s)
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
}

// ---------------------------------------------------------------------------
// Asset

#[derive(Debug, Clone)]
pub struct Asset {
    bytes: Vec<u8>,
    header: Header,
    types: Vec<TypeEntry>,
    layout: Layout,
    spans: Vec<Span>,
    properties: Vec<CustomProperty>,
    // Every value decoded through the class schema (no fallback scan).
    typed: bool,
    // Wire property lists for this file's class versions.
    schema: HashMap<u32, Vec<&'static Prop>>,
    // crc32(text) -> first Str span, for per-file self-names.
    self_names: HashMap<u32, usize>,
    // Every field's first span, sorted by (object, prop): what reading an
    // object's properties looks up, built once instead of per read.
    field_spans: Vec<FieldSpan>,
    // The root object's id: asked for on every template expansion.
    guid: Option<Guid>,
    // An EnumAsset's conversion rules, read on first use (bindings ask
    // for a rule's value every time they run).
    enum_rules: std::sync::OnceLock<Vec<EnumRule>>,
    // See `Asset::widgets`.
    widgets: WidgetCache,
}

/// [`Asset::widgets`] as last read, with the palette generation it was
/// read at.
#[derive(Debug, Default)]
struct WidgetCache(std::sync::Mutex<Option<(u64, std::sync::Arc<[Widget]>)>>);

impl Clone for WidgetCache {
    fn clone(&self) -> WidgetCache {
        WidgetCache(std::sync::Mutex::new(
            self.0.lock().expect("widget cache").clone(),
        ))
    }
}

/// An EnumAsset ConversionRule: its RuleId, then per ConversionValue in
/// file order its entry UniqueId and the value it stores.
type EnumRule = (u32, Vec<(Option<u32>, Option<DefaultValue>)>);

/// A field's first span: (object, prop, span), packed (an asset has one
/// per value; a file's span indices fit 32 bits).
type FieldSpan = (u32, u16, u32);

/// Each field's first span, sorted by (object, prop).
fn index_fields(spans: &[Span]) -> Vec<FieldSpan> {
    let mut out: Vec<FieldSpan> = spans
        .iter()
        .enumerate()
        .filter_map(|(i, s)| {
            let f = s.field?;
            Some((f.object as u32, f.prop, i as u32))
        })
        .collect();
    // Equal fields sort by span, so dedup keeps the first.
    out.sort_unstable();
    out.dedup_by_key(|e| (e.0, e.1));
    out
}

/// A borrowed view of [`Asset`]'s field index.
#[derive(Clone, Copy)]
pub(crate) struct Fields<'a>(&'a [FieldSpan]);

impl Fields<'_> {
    pub(crate) fn get(&self, f: &Field) -> Option<usize> {
        let key = (u32::try_from(f.object).ok()?, f.prop);
        let i = self.0.binary_search_by_key(&key, |e| (e.0, e.1)).ok()?;
        Some(self.0[i].2 as usize)
    }
}

/// A widget-tree node: the root or an object with an id.
#[derive(Debug, Clone)]
pub struct Node<'a> {
    pub span: usize,
    pub header: &'a ObjectHeader,
    /// Span indices after the header, up to the next node.
    pub records: Range<usize>,
}

/// Other assets and images this one points at.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CrossRefs {
    /// References to ids that are not objects of this file: (class of the
    /// target, guid, occurrences). The class is the target kind's root class
    /// (ImageAsset, EnumAsset, ComponentTemplate, ContainerWidgetTemplate…).
    pub assets: Vec<(u32, Guid, usize)>,
    /// Image paths stored as strings (`data\ui\…\x.png`), deduplicated.
    pub images: Vec<String>,
}

/// An edit the UI editor may offer.
#[derive(Debug, Clone, PartialEq)]
pub enum Edit {
    /// Replace a string record's text; its crc prefix (if any) is recomputed.
    Text { span: usize, text: String },
    /// Replace a Color's RGB.
    Color { span: usize, rgb: [u8; 3] },
    /// Replace a Bool/S32/U32/F32 custom property default with one of the
    /// same type.
    Default { span: usize, value: DefaultValue },
    /// Replace a scalar property value ([`Record::Value`]) with one of the
    /// same variant (and, for enums, the same enum type). Fixed width, so the
    /// file length never changes.
    Value { span: usize, value: Value },
}

#[derive(Debug, Clone)]
pub struct Edited {
    pub asset: Asset,
    /// crc32 of the replaced text, for crc-prefixed strings.
    pub old_crc: Option<u32>,
    /// Offsets in the new file where `old_crc` still appears outside the
    /// edited record: something else (a binding, a property id) keys on the
    /// old name. Other assets can hold it too (widgets bind component
    /// properties by name crc); check them with [`Asset::find_u32`].
    pub crc_refs: Vec<usize>,
}

impl Asset {
    pub fn parse(bytes: Vec<u8>) -> Result<Asset> {
        ensure!(
            bytes.len() > HEADER_LEN,
            "PHX asset too short ({} bytes)",
            bytes.len()
        );
        ensure!(
            &bytes[..3] == b"PHX",
            "not a PHX asset (magic {:02x?})",
            &bytes[..5]
        );
        let header = Header(bytes[..HEADER_LEN].try_into().unwrap());

        let (types, types_end) = parse_types(&bytes)?;
        ensure!(!types.is_empty(), "class table is empty");
        let (layout, layout_end) = parse_layout(&bytes, types_end, types.len())?;

        // About one span per 7 bytes (font data aside, which is one): one
        // reserve with room, shrunk once decoded, instead of growing
        // through a dozen copies.
        let mut head = Vec::with_capacity(bytes.len() / 4 + 16);
        head.extend([
            Span::new(0..HEADER_LEN, Record::Header),
            Span::new(HEADER_LEN..types_end, Record::Types),
            Span::new(types_end..layout_end, Record::Layout),
        ]);
        let schema = file_schema(&types).unwrap_or_default();
        // The body decodes into the span list after the header sections,
        // not into one of its own to be copied over.
        let mut wire = Wire::new(&bytes, &types, &schema, head);
        let parsed = (!schema.is_empty())
            .then(|| wire.parse(layout_end))
            .flatten();
        let (mut spans, properties, typed) = match parsed {
            Some(()) => (wire.spans, wire.properties, true),
            None => {
                let mut spans = wire.spans;
                spans.truncate(3);
                let mut body = Body::new(&bytes, &types);
                body.parse(layout_end, &layout)?;
                spans.append(&mut body.spans);
                (spans, Vec::new(), false)
            }
        };
        merge_opaque(&mut spans);
        spans.shrink_to_fit();

        let mut self_names = HashMap::new();
        for (i, s) in spans.iter().enumerate() {
            if let Record::Str(r) = &s.record {
                self_names.entry(r.crc()).or_insert(i);
            }
        }
        let field_spans = index_fields(&spans);
        let mut asset = Asset {
            bytes,
            header,
            types,
            layout,
            spans,
            properties,
            typed,
            schema,
            self_names,
            field_spans,
            guid: None,
            enum_rules: std::sync::OnceLock::new(),
            widgets: WidgetCache::default(),
        };
        let guid = asset.objects().find_map(|(_, o)| match o.id {
            ObjectId::Root(g) => Some(g),
            _ => None,
        });
        asset.guid = guid;
        // Every record decoder only accepts canonical encodings, so this holds
        // by construction; checking it keeps `encode` honest on hostile input.
        ensure!(
            asset.encode() == asset.bytes,
            "decoded records do not re-encode to the input"
        );
        Ok(asset)
    }

    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }
    pub fn header(&self) -> &Header {
        &self.header
    }
    pub fn types(&self) -> &[TypeEntry] {
        &self.types
    }
    pub fn layout(&self) -> &Layout {
        &self.layout
    }
    /// Every byte of the file lies in exactly one span, in file order.
    pub fn spans(&self) -> &[Span] {
        &self.spans
    }
    pub fn properties(&self) -> &[CustomProperty] {
        &self.properties
    }
    /// True when every value was decoded through the class schema; false
    /// when a class is missing from it and the fallback scan ran (then
    /// [`Asset::properties`] and [`Asset::widgets`] are empty).
    pub fn typed(&self) -> bool {
        self.typed
    }
    /// Wire property list of `class` as this file wrote it (older class
    /// versions included). Index `i` is presence-mask bit `i + 1`.
    pub fn class_props(&self, class: u32) -> Option<&[&'static Prop]> {
        self.schema.get(&class).map(|v| v.as_slice())
    }

    pub fn root_class(&self) -> u32 {
        self.types[0].class
    }
    /// Schema hash of the root class (the doc's per-kind "schema guid").
    pub fn schema(&self) -> [u8; 16] {
        self.types[0].schema
    }
    /// The guid in the root object header; equals the file name.
    pub fn guid(&self) -> Option<Guid> {
        self.guid
    }

    pub fn objects(&self) -> impl Iterator<Item = (usize, &ObjectHeader)> {
        self.spans
            .iter()
            .enumerate()
            .filter_map(|(i, s)| match &s.record {
                Record::Object(o) => Some((i, o)),
                _ => None,
            })
    }

    pub fn strings(&self) -> impl Iterator<Item = (usize, &StrRecord)> {
        self.spans
            .iter()
            .enumerate()
            .filter_map(|(i, s)| match &s.record {
                Record::Str(r) => Some((i, r)),
                _ => None,
            })
    }

    pub fn references(&self) -> impl Iterator<Item = (usize, &Reference)> {
        self.spans
            .iter()
            .enumerate()
            .filter_map(|(i, s)| match &s.record {
                Record::Ref(r) => Some((i, r)),
                _ => None,
            })
    }

    /// The root plus every object that carries an id, in file order. General
    /// parent/child nesting is not recoverable without the per-class schema;
    /// [`Asset::root_children`] is the one child count that is.
    pub fn nodes(&self) -> Vec<Node<'_>> {
        let heads: Vec<(usize, &ObjectHeader)> = self
            .objects()
            .filter(|(_, o)| !matches!(o.id, ObjectId::None))
            .collect();
        heads
            .iter()
            .enumerate()
            .map(|(k, &(span, header))| {
                let end = heads.get(k + 1).map_or(self.spans.len(), |n| n.0);
                Node {
                    span,
                    header,
                    records: span + 1..end,
                }
            })
            .collect()
    }

    /// Size of the array in the root's first pointer slot, when the layout
    /// starts `40 root, 42 n`. For widget templates this is the number of
    /// top-level widgets (`… <mask> 01 00 00 00 <n> 02 02 <id> …`).
    pub fn root_children(&self) -> Option<u32> {
        match self.layout.tokens.get(1) {
            Some(t) if t.kind == TOKEN_ARRAY => Some(t.count),
            _ => None,
        }
    }

    /// Resolves a crc through `names`, then through this file's own strings.
    pub fn name<'a>(&'a self, names: &'a Names, crc: u32) -> Option<&'a str> {
        names.get(crc).or_else(|| {
            let &i = self.self_names.get(&crc)?;
            match &self.spans[i].record {
                Record::Str(r) => Some(r.text.as_str()),
                _ => None,
            }
        })
    }

    pub fn property_name(&self, p: &CustomProperty) -> &str {
        match &self.spans[p.name].record {
            Record::Str(r) => &r.text,
            other => unreachable!("property name span holds {other:?}"),
        }
    }
    pub fn property_default(&self, p: &CustomProperty) -> &DefaultValue {
        match &self.spans[p.default].record {
            Record::Default(v) => v,
            other => unreachable!("property default span holds {other:?}"),
        }
    }

    pub fn cross_refs(&self) -> CrossRefs {
        let local: HashSet<Guid> = self
            .objects()
            .filter_map(|(_, o)| match o.id {
                ObjectId::Root(g) | ObjectId::Id(g) => Some(g),
                ObjectId::None => None,
            })
            .collect();
        let mut assets: BTreeMap<(u32, Guid), usize> = BTreeMap::new();
        for (_, r) in self.references() {
            if !local.contains(&r.id) {
                *assets.entry((r.class, r.id)).or_default() += 1;
            }
        }
        let mut images: Vec<String> = Vec::new();
        let defaults = self.spans.iter().filter_map(|s| match &s.record {
            Record::Default(DefaultValue::Str(t)) => Some(t),
            _ => None,
        });
        for text in self.strings().map(|(_, s)| &s.text).chain(defaults) {
            if is_image_path(text) && !images.contains(text) {
                images.push(text.clone());
            }
        }
        CrossRefs {
            assets: assets.into_iter().map(|((c, g), n)| (c, g, n)).collect(),
            images,
        }
    }

    /// Offsets of every little-endian occurrence of `value` in the file.
    pub fn find_u32(&self, value: u32) -> Vec<usize> {
        let needle = value.to_le_bytes();
        self.bytes
            .windows(4)
            .enumerate()
            .filter(|(_, w)| *w == needle)
            .map(|(i, _)| i)
            .collect()
    }

    /// Rebuilds the file from the decoded records; opaque spans are copied.
    pub fn encode(&self) -> Vec<u8> {
        self.encode_with(None)
    }

    fn encode_with(&self, replace: Option<(usize, &Record)>) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.bytes.len() + 64);
        for (i, span) in self.spans.iter().enumerate() {
            let record = match replace {
                Some((j, r)) if j == i => r,
                _ => &span.record,
            };
            self.encode_record(record, &span.range, &mut out);
        }
        out
    }

    fn encode_with_many(&self, replacements: &[(usize, Record)]) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.bytes.len() + 64);
        for (i, span) in self.spans.iter().enumerate() {
            let record = replacements
                .iter()
                .find(|(j, _)| *j == i)
                .map(|(_, r)| r)
                .unwrap_or(&span.record);
            self.encode_record(record, &span.range, &mut out);
        }
        out
    }

    fn encode_record(&self, record: &Record, range: &Range<usize>, out: &mut Vec<u8>) {
        match record {
            Record::Header => out.extend_from_slice(&self.header.0),
            Record::Types => encode_types(&self.types, out),
            Record::Layout => encode_layout(&self.layout, out),
            Record::Object(o) => encode_object(o, out),
            Record::Str(s) => {
                if s.with_crc {
                    out.extend_from_slice(&s.crc().to_le_bytes());
                }
                write_str_payload(out, &s.text);
            }
            Record::Ref(r) => {
                out.push(1);
                out.extend_from_slice(&r.class.to_le_bytes());
                out.extend_from_slice(&r.id.0);
            }
            Record::Color(rgb) => out.extend_from_slice(rgb),
            Record::Value(v) => v.encode(out),
            Record::Null => out.push(0),
            Record::Count(n) => write_varint(out, *n),
            Record::Default(v) => v.encode(out),
            Record::Bytes | Record::Opaque => out.extend_from_slice(&self.bytes[range.clone()]),
        }
    }

    /// Applies one edit and returns the re-parsed asset. The edit is rejected
    /// unless the result parses to the same span structure with every other
    /// span byte-identical.
    ///
    /// Length changes are safe in the container: there are no offset tables
    /// or size fields (header bytes 8..16 are constant, the layout byte count
    /// covers only the layout section, string lengths are per-record varints).
    pub fn apply(&self, edit: &Edit) -> Result<Edited> {
        self.apply_many(std::slice::from_ref(edit))
    }

    /// Applies several edits in one encode+parse round, under the same
    /// per-edit guarantees as [`Asset::apply`]: the span structure is
    /// preserved, edited spans re-parse to the new records, and every other
    /// span stays byte-identical. No two edits may target the same span.
    /// `old_crc`/`crc_refs` report the first text edit that changes a CRC'd
    /// string (the one consumers of a single `apply` would see).
    pub fn apply_many(&self, edits: &[Edit]) -> Result<Edited> {
        if edits.is_empty() {
            bail!("no edits to apply");
        }
        let mut replacements: Vec<(usize, Record)> = Vec::with_capacity(edits.len());
        for edit in edits {
            replacements.push(self.edited_record(edit)?);
        }
        {
            let mut spans: Vec<usize> = replacements.iter().map(|(s, _)| *s).collect();
            spans.sort_unstable();
            spans.dedup();
            ensure!(
                spans.len() == replacements.len(),
                "two edits target the same span"
            );
        }
        let old_crc = replacements.iter().find_map(|(span, record)| {
            match (&self.spans[*span].record, record) {
                (Record::Str(old), Record::Str(new)) if old.with_crc && old.text != new.text => {
                    Some(old.crc())
                }
                _ => None,
            }
        });
        let bytes = self.encode_with_many(&replacements);
        let next = Asset::parse(bytes).context("edited asset no longer parses")?;
        ensure!(
            next.spans.len() == self.spans.len(),
            "edit changes the decoded structure ({} spans -> {})",
            self.spans.len(),
            next.spans.len()
        );
        for (i, (a, b)) in self.spans.iter().zip(&next.spans).enumerate() {
            ensure!(
                a.record.same_kind(&b.record) && a.field == b.field,
                "edit changes the kind of span {i} at {:#x}",
                a.range.start
            );
            if let Some((_, record)) = replacements.iter().find(|(s, _)| *s == i) {
                ensure!(
                    b.record == *record,
                    "edited span {i} re-parses to a different value"
                );
            } else {
                ensure!(
                    self.bytes[a.range.clone()] == next.bytes[b.range.clone()],
                    "edit disturbs span {i} at {:#x}",
                    a.range.start
                );
            }
        }
        let crc_refs = match old_crc {
            Some(crc) => {
                let own: Vec<usize> = replacements
                    .iter()
                    .filter(|(span, record)| {
                        matches!(&self.spans[*span].record, Record::Str(_))
                            && matches!(record, Record::Str(_))
                    })
                    .map(|(span, _)| next.spans[*span].range.start)
                    .collect();
                next.find_u32(crc)
                    .into_iter()
                    .filter(|&o| !own.contains(&o))
                    .collect()
            }
            None => Vec::new(),
        };
        Ok(Edited {
            asset: next,
            old_crc,
            crc_refs,
        })
    }

    /// First byte of the body (the root object).
    pub fn body_start(&self) -> usize {
        self.spans[2].range.end
    }

    /// Applies structural splices to the body (objects added, removed or
    /// moved; array counts changed) and re-derives what depends on it: the
    /// class table's heap instance counts and the layout section. `classes`
    /// adds class table entries (with their base chains) for objects of
    /// classes this file did not use before.
    ///
    /// The layout section is a function of the body, checked against all
    /// 4,682 installed assets: in file order, `0x40 1 <class index>` per heap
    /// object and `<element type code> n` per non-empty array (`0x42` pointer
    /// arrays, `0x08` u32 arrays), plus after a binding's target array one
    /// `0x51 n` run each for its source and target index arrays. The per-kind
    /// sums keep the file's order (descending kind in all but one shipped
    /// file); new kinds go last.
    // TODO(alex): new class table entries are appended; the serializer adds
    // them in first-use order. Not known whether the runtime cares.
    pub fn rebuild(&self, splices: &[Splice], classes: &[u32]) -> Result<Asset> {
        let start = self.body_start();
        let mut sorted: Vec<&Splice> = splices.iter().collect();
        sorted.sort_by_key(|s| (s.range.start, s.range.end));
        let mut body = Vec::with_capacity(self.bytes.len());
        let mut at = start;
        for s in sorted {
            ensure!(
                s.range.start >= at && s.range.end <= self.bytes.len(),
                "splices overlap or leave the body ({:?})",
                s.range
            );
            body.extend_from_slice(&self.bytes[at..s.range.start]);
            body.extend_from_slice(&s.bytes);
            at = s.range.end;
        }
        body.extend_from_slice(&self.bytes[at..]);

        let mut types = self.types.clone();
        for &c in classes {
            let mut chain = vec![c];
            while let Some(b) = base_of(*chain.last().unwrap()) {
                chain.push(b);
            }
            for c in chain {
                if types.iter().any(|t| t.class == c) {
                    continue;
                }
                let info = class_info(c).with_context(|| format!("class {c:08x} is unknown"))?;
                types.push(TypeEntry {
                    class: c,
                    schema: info.hash,
                    instances: 0,
                });
            }
        }
        let schema = file_schema(&types).context("a class has no schema")?;
        let mut wire = Wire::new(&body, &types, &schema, Vec::new());
        wire.recount = true;
        wire.parse(0)
            .context("the edited body does not decode as objects")?;
        for t in &mut types {
            t.instances = wire.seen.get(&t.class).copied().unwrap_or(0);
        }

        let index: HashMap<u32, u32> = types
            .iter()
            .enumerate()
            .map(|(i, t)| (t.class, i as u32))
            .collect();
        let class_of = |object: usize| match &wire.spans[object].record {
            Record::Object(o) => o.class,
            _ => unreachable!("field owner is an object"),
        };
        let mut tokens = Vec::new();
        for s in &wire.spans {
            match &s.record {
                Record::Object(o) if o.heap => tokens.push(LayoutToken {
                    kind: TOKEN_OBJECT,
                    count: 1,
                    class_index: Some(index[&o.class]),
                }),
                Record::Count(n) => {
                    let f = s.field.expect("counts belong to a field");
                    let class = class_of(f.object);
                    let prop = schema[&class][f.prop as usize - 1];
                    if *n > 0 {
                        tokens.push(LayoutToken {
                            kind: (prop.ty >> 8) as u8,
                            count: *n,
                            class_index: None,
                        });
                    }
                    if prop.name_crc == PROP_BINDING_TARGETS {
                        let sources = wire.spans.iter().find_map(|t| match (&t.record, t.field) {
                            (Record::Count(k), Some(g))
                                if g.object == f.object
                                    && schema[&class][g.prop as usize - 1].name_crc
                                        == PROP_BINDING_SOURCES =>
                            {
                                Some(*k)
                            }
                            _ => None,
                        });
                        for k in [sources.unwrap_or(0), *n] {
                            if k > 0 {
                                tokens.push(LayoutToken {
                                    kind: TOKEN_BINDING_INDICES,
                                    count: k,
                                    class_index: None,
                                });
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        let mut sums: BTreeMap<u8, u32> = BTreeMap::new();
        for t in tokens.iter().filter(|t| t.kind != TOKEN_OBJECT) {
            *sums.entry(t.kind).or_default() += t.count;
        }
        let mut counts: Vec<(u8, u32)> = self
            .layout
            .counts
            .iter()
            .filter_map(|&(k, _)| Some((k, sums.remove(&k)?)))
            .collect();
        counts.extend(sums.into_iter().rev());
        let layout = Layout {
            counts,
            reserved: self.layout.reserved,
            tokens,
        };

        let mut out = Vec::with_capacity(body.len() + start);
        out.extend_from_slice(&self.header.0);
        encode_types(&types, &mut out);
        encode_layout(&layout, &mut out);
        out.extend_from_slice(&body);
        let next = Asset::parse(out).context("the rebuilt asset does not parse")?;
        ensure!(next.typed, "the rebuilt asset is not fully typed");
        Ok(next)
    }

    fn edited_record(&self, edit: &Edit) -> Result<(usize, Record)> {
        let span_of = |i: usize| {
            self.spans
                .get(i)
                .with_context(|| format!("span {i} out of range ({} spans)", self.spans.len()))
        };
        match edit {
            Edit::Text { span, text } => {
                let Record::Str(old) = &span_of(*span)?.record else {
                    bail!("span {span} is not a string record");
                };
                // The scanner must find the new text again as the same record.
                let ok = if old.with_crc {
                    valid_crc_text(text)
                } else {
                    valid_bare_text(text)
                };
                ensure!(
                    ok,
                    "text {text:?} would not re-parse as a string here (empty, control bytes, \
                     or under 4 bytes / no letter for a crc-less string)"
                );
                Ok((
                    *span,
                    Record::Str(StrRecord {
                        with_crc: old.with_crc,
                        text: text.clone(),
                    }),
                ))
            }
            Edit::Color { span, rgb } => {
                let Record::Color(_) = span_of(*span)?.record else {
                    bail!("span {span} is not a color");
                };
                Ok((*span, Record::Color(*rgb)))
            }
            Edit::Default { span, value } => {
                let Record::Default(old) = &span_of(*span)?.record else {
                    bail!("span {span} is not a custom property default");
                };
                ensure!(
                    old.same_editable_kind(value),
                    "only Bool/S32/U32/F32, vector and text defaults are editable, and only \
                     within the same type and length ({old:?} -> {value:?})"
                );
                Ok((*span, Record::Default(value.clone())))
            }
            Edit::Value { span, value } => {
                let Record::Value(old) = &span_of(*span)?.record else {
                    bail!("span {span} is not a scalar property value");
                };
                ensure!(
                    old.same_kind(value),
                    "a {old:?} value can only be replaced by the same type, not {value:?}"
                );
                Ok((*span, Record::Value(value.clone())))
            }
        }
    }
}

fn is_image_path(s: &str) -> bool {
    let l = s.to_ascii_lowercase();
    [".png", ".dds", ".tga", ".jpg", ".jpeg"]
        .iter()
        .any(|e| l.ends_with(e))
}

// ---------------------------------------------------------------------------
// Header sections

// LEB128, canonical only: a padded form (`85 00` for 5) or one that overflows
// u32 is rejected, so every decoded record re-encodes to its own bytes.
fn read_varint(b: &[u8], mut p: usize) -> Option<(u32, usize)> {
    let mut v: u32 = 0;
    for shift in (0..35).step_by(7) {
        let x = *b.get(p)?;
        p += 1;
        if (shift == 28 && x > 0x0f) || (shift > 0 && x == 0) {
            return None;
        }
        v |= u32::from(x & 0x7f) << shift;
        if x < 0x80 {
            return Some((v, p));
        }
    }
    None
}

fn write_varint(out: &mut Vec<u8>, mut v: u32) {
    while v >= 0x80 {
        out.push((v as u8) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

fn u32_at(b: &[u8], p: usize) -> Option<u32> {
    Some(u32::from_le_bytes(
        b.get(p..p.checked_add(4)?)?.try_into().unwrap(),
    ))
}

fn guid_at(b: &[u8], p: usize) -> Option<Guid> {
    Some(Guid(b.get(p..p.checked_add(16)?)?.try_into().unwrap()))
}

fn write_str_payload(out: &mut Vec<u8>, s: &str) {
    write_varint(out, s.len() as u32);
    if !s.is_empty() {
        out.push(str_encoding(s));
        out.extend_from_slice(s.as_bytes());
    }
}

// The byte after a non-empty string's length: `01` for ASCII, `03` for text
// with non-ASCII UTF-8 (2 shipped strings, both mojibake). Empty strings are
// the bare `00` length.
fn str_encoding(s: &str) -> u8 {
    if s.is_ascii() { 1 } else { 3 }
}

// `<varint len> <encoding> <bytes>` at `p`; the encoding byte must be the one
// write_str_payload would emit so the record re-encodes exactly.
fn read_str(b: &[u8], p: usize) -> Option<(String, usize)> {
    let (n, q) = read_varint(b, p)?;
    if n == 0 {
        return Some((String::new(), q));
    }
    let end = (q + 1).checked_add(n as usize)?;
    let text = std::str::from_utf8(b.get(q + 1..end)?).ok()?;
    (*b.get(q)? == str_encoding(text)).then(|| (text.to_owned(), end))
}

fn parse_types(b: &[u8]) -> Result<(Vec<TypeEntry>, usize)> {
    let (n, mut p) = read_varint(b, HEADER_LEN).context("class table count")?;
    ensure!(n <= MAX_TYPES, "class table claims {n} entries");
    let mut types = Vec::with_capacity(n as usize);
    for i in 0..n {
        let truncated = || format!("class table entry {i} truncated");
        let class = u32_at(b, p).with_context(truncated)?;
        let schema: [u8; 16] = b
            .get(p + 4..p + 20)
            .with_context(truncated)?
            .try_into()
            .unwrap();
        let (instances, q) = read_varint(b, p + 20).with_context(truncated)?;
        types.push(TypeEntry {
            class,
            schema,
            instances,
        });
        p = q;
    }
    let mut check = Vec::new();
    encode_types(&types, &mut check);
    ensure!(
        check == b[HEADER_LEN..p],
        "class table uses non-canonical varints"
    );
    Ok((types, p))
}

fn encode_types(types: &[TypeEntry], out: &mut Vec<u8>) {
    write_varint(out, types.len() as u32);
    for t in types {
        out.extend_from_slice(&t.class.to_le_bytes());
        out.extend_from_slice(&t.schema);
        write_varint(out, t.instances);
    }
}

fn parse_layout(b: &[u8], start: usize, n_types: usize) -> Result<(Layout, usize)> {
    let bad = || format!("layout section at {start:#x} truncated");
    let (k, mut p) = read_varint(b, start).with_context(bad)?;
    ensure!(k <= 256, "layout claims {k} count kinds");
    let mut counts = Vec::with_capacity(k as usize);
    for _ in 0..k {
        let kind = *b.get(p).with_context(bad)?;
        let (n, q) = read_varint(b, p + 1).with_context(bad)?;
        counts.push((kind, n));
        p = q;
    }
    let reserved = *b.get(p).with_context(bad)?;
    let (n_tokens, q) = read_varint(b, p + 1).with_context(bad)?;
    let (n_bytes, q) = read_varint(b, q).with_context(bad)?;
    ensure!(
        n_tokens <= MAX_LAYOUT_TOKENS,
        "layout claims {n_tokens} tokens"
    );
    let end = q
        .checked_add(n_bytes as usize)
        .filter(|&e| e <= b.len())
        .with_context(bad)?;
    let mut tokens = Vec::with_capacity(n_tokens as usize);
    let mut p = q;
    while p < end {
        let kind = b[p];
        let (count, q) = read_varint(b, p + 1)
            .filter(|x| x.1 <= end)
            .with_context(bad)?;
        p = q;
        let class_index = if kind == TOKEN_OBJECT {
            let (ci, q) = read_varint(b, p).filter(|x| x.1 <= end).with_context(bad)?;
            ensure!(
                (ci as usize) < n_types,
                "layout token names class {ci} of {n_types}"
            );
            p = q;
            Some(ci)
        } else {
            None
        };
        tokens.push(LayoutToken {
            kind,
            count,
            class_index,
        });
    }
    ensure!(
        tokens.len() == n_tokens as usize,
        "layout has {} tokens, its header says {n_tokens}",
        tokens.len()
    );
    let layout = Layout {
        counts,
        reserved,
        tokens,
    };
    let mut check = Vec::new();
    encode_layout(&layout, &mut check);
    ensure!(
        check == b[start..end],
        "layout section uses non-canonical varints"
    );
    Ok((layout, end))
}

fn encode_layout(layout: &Layout, out: &mut Vec<u8>) {
    write_varint(out, layout.counts.len() as u32);
    for &(kind, n) in &layout.counts {
        out.push(kind);
        write_varint(out, n);
    }
    out.push(layout.reserved);
    let mut body = Vec::new();
    for t in &layout.tokens {
        body.push(t.kind);
        write_varint(&mut body, t.count);
        if let Some(ci) = t.class_index {
            write_varint(&mut body, ci);
        }
    }
    write_varint(out, layout.tokens.len() as u32);
    write_varint(out, body.len() as u32);
    out.extend_from_slice(&body);
}

fn encode_object(o: &ObjectHeader, out: &mut Vec<u8>) {
    out.push(2);
    match o.id {
        ObjectId::Root(g) => out.extend_from_slice(&g.0),
        ObjectId::None => out.push(0),
        ObjectId::Id(g) => {
            out.push(2);
            out.extend_from_slice(&g.0);
        }
    }
    out.extend_from_slice(&o.class.to_le_bytes());
    out.push(o.prop_count);
    out.extend_from_slice(&o.mask);
}

// ---------------------------------------------------------------------------
// Body

// Reads `<count> <mask>` at `p`. Counts stay below 0x80 in the shipped files
// (max 0x26); a larger one would be a varint we have never seen, so it is
// rejected. Mask bits past the count must be clear.
fn read_counts(b: &[u8], p: usize) -> Option<(u8, Box<[u8]>, usize)> {
    let count = *b.get(p)?;
    if count == 0 || count >= 0x80 {
        return None;
    }
    let mlen = (count as usize).div_ceil(8);
    let mask = b.get(p + 1..p + 1 + mlen)?;
    let rem = count % 8;
    if rem != 0 && mask[mlen - 1] & ((1u8 << (8 - rem)) - 1) != 0 {
        return None;
    }
    Some((count, mask.into(), p + 1 + mlen))
}

fn valid_crc_text(s: &str) -> bool {
    !s.is_empty()
        && !s
            .chars()
            .any(|c| c.is_control() && !matches!(c, '\t' | '\r' | '\n'))
}

// Crc-less strings carry no checksum, so the scanner wants them to look like
// text: printable ASCII, a letter, at least 4 bytes.
fn valid_bare_text(s: &str) -> bool {
    s.len() >= 4
        && s.len() <= MAX_SCAN_STR
        && s.bytes()
            .all(|c| (0x20..0x7f).contains(&c) || matches!(c, b'\t' | b'\r' | b'\n'))
        && s.bytes().any(|c| c.is_ascii_alphabetic())
}

fn merge_opaque(spans: &mut Vec<Span>) {
    // Only the fallback scan leaves these; a schema-decoded file has none.
    if !spans
        .iter()
        .any(|s| s.range.is_empty() || s.record == Record::Opaque)
    {
        return;
    }
    let mut kept = 0;
    for i in 0..spans.len() {
        if spans[i].range.is_empty() {
            continue;
        }
        if kept > 0
            && spans[i].record == Record::Opaque
            && spans[kept - 1].record == Record::Opaque
            && spans[kept - 1].range.end == spans[i].range.start
        {
            spans[kept - 1].range.end = spans[i].range.end;
            continue;
        }
        spans.swap(kept, i);
        kept += 1;
    }
    spans.truncate(kept);
}

const PROP_UNIQUE_ID: u32 = name_crc(b"UniqueId");
const PROP_PROPERTY_TYPE: u32 = name_crc(b"PropertyType");
const PROP_DEFAULT_VALUE: u32 = name_crc(b"DefaultValue");
const PROP_OBJECT_TEMPLATE: u32 = name_crc(b"ObjectTemplate");
const PROP_ENUM_ASSET: u32 = name_crc(b"EnumAsset");
// The name string of named objects (widgets, custom properties); its
// `NameCrc` u32 sits right before it.
const PROP_NAME: u32 = name_crc(b"PhoenixDebugName");

// Schema-driven body decode: the root object, then every property value in
// wire order, recursing through pointers. All-or-nothing: None means some
// class or value shape is not covered and the caller falls back to Body.
struct Wire<'a> {
    b: &'a [u8],
    schema: &'a HashMap<u32, Vec<&'static Prop>>,
    // Classes the class table counts heap instances for, with the count.
    heap: HashMap<u32, u32>,
    // Rebuilding after a splice: heap-ness comes from VALUE_CLASSES and the
    // counts are whatever the body holds (`seen`), not checked.
    recount: bool,
    seen: HashMap<u32, u32>,
    // Every span so far, from what the caller seeded (the header sections):
    // indices are the asset's.
    spans: Vec<Span>,
    properties: Vec<CustomProperty>,
}

impl<'a> Wire<'a> {
    fn new(
        b: &'a [u8],
        types: &[TypeEntry],
        schema: &'a HashMap<u32, Vec<&'static Prop>>,
        spans: Vec<Span>,
    ) -> Wire<'a> {
        let heap = types
            .iter()
            .filter(|t| t.instances > 0)
            .map(|t| (t.class, t.instances))
            .collect();
        Wire {
            b,
            schema,
            heap,
            recount: false,
            seen: HashMap::new(),
            spans,
            properties: Vec::new(),
        }
    }

    fn push(&mut self, range: Range<usize>, record: Record, field: Option<Field>) -> usize {
        self.spans.push(Span {
            range,
            record,
            field,
        });
        self.spans.len() - 1
    }

    fn parse(&mut self, start: usize) -> Option<()> {
        let b = self.b;
        if *b.get(start)? != 2 {
            return None;
        }
        let id = ObjectId::Root(guid_at(b, start + 1)?);
        let end = self.object_at(start, start + 17, id, None)?;
        // Heap objects must match the class table counts one for one.
        (end == b.len() && (self.recount || self.seen == self.heap)).then_some(())
    }

    // Header from the class crc at `c` on (`start` is where `02` sits), then
    // the values.
    fn object_at(
        &mut self,
        start: usize,
        c: usize,
        id: ObjectId,
        field: Option<Field>,
    ) -> Option<usize> {
        let class = u32_at(self.b, c)?;
        let (prop_count, mask, e) = read_counts(self.b, c + 4)?;
        let heap = if self.recount {
            !VALUE_CLASSES.contains(&class)
        } else {
            self.heap.contains_key(&class)
        };
        if heap {
            *self.seen.entry(class).or_default() += 1;
        }
        let header = ObjectHeader {
            id,
            class,
            prop_count,
            mask,
            heap,
        };
        let schema = self.schema;
        let props = schema.get(&class)?.as_slice();
        if props.len() + 1 != prop_count as usize || header.present(0) {
            return None;
        }
        let present: Vec<usize> = (1..prop_count as usize)
            .filter(|&i| header.present(i))
            .collect();
        let obj = self.push(start..e, Record::Object(header), field);
        let mut p = e;
        if class == CLASS_COLOR && present == [1, 2, 3] {
            // One span for R, G, B so a color edit is one record.
            let rgb = self.b.get(p..p + 3)?.try_into().unwrap();
            self.push(
                p..p + 3,
                Record::Color(rgb),
                Some(Field {
                    object: obj,
                    prop: 1,
                }),
            );
            return Some(p + 3);
        }
        for i in present {
            let field = Field {
                object: obj,
                prop: i as u16,
            };
            p = self.value(p, field, props[i - 1])?;
        }
        if class == CLASS_CUSTOM_PROPERTY {
            self.custom_property(obj, props);
        }
        Some(p)
    }

    fn value(&mut self, p: usize, field: Field, prop: &Prop) -> Option<usize> {
        if prop.ty & 1 == 0 {
            return self.element(p, field, prop.ty, prop);
        }
        let (n, mut q) = read_varint(self.b, p)?;
        if n as usize > self.b.len() - q {
            return None;
        }
        self.push(p..q, Record::Count(n), Some(field));
        if prop.ty & !1 == 0x0400 {
            let end = q + n as usize;
            if n > 0 {
                self.push(q..end, Record::Bytes, Some(field));
            }
            return Some(end);
        }
        for _ in 0..n {
            q = self.element(q, field, prop.ty & !1, prop)?;
        }
        Some(q)
    }

    fn element(&mut self, p: usize, field: Field, ty: u16, prop: &Prop) -> Option<usize> {
        let b = self.b;
        let f32_at = |i: usize| u32_at(b, p + 4 * i).map(f32::from_bits);
        let (value, width) = match ty {
            0x0100 => match *b.get(p)? {
                v @ (0 | 1) => (Value::Bool(v == 1), 1),
                v => (Value::U8(v), 1),
            },
            0x0400 => (Value::U8(*b.get(p)?), 1),
            0x0600 => (
                Value::U16(u16::from_le_bytes(b.get(p..p + 2)?.try_into().unwrap())),
                2,
            ),
            0x0700 => (Value::S32(u32_at(b, p)? as i32), 4),
            0x0800 => (Value::U32(u32_at(b, p)?), 4),
            0x0a00 => (
                Value::U64(u64::from_le_bytes(b.get(p..p + 8)?.try_into().unwrap())),
                8,
            ),
            0x0b00 => (
                Value::Enum {
                    ty: prop.type_crc,
                    value: u32_at(b, p)?,
                },
                4,
            ),
            0x1000 => (Value::F32(f32_at(0)?), 4),
            0x1100 => (Value::Vec2([f32_at(0)?, f32_at(1)?]), 8),
            0x1200 => (Value::Vec3([f32_at(0)?, f32_at(1)?, f32_at(2)?]), 12),
            0x1300 => (
                Value::Vec4([f32_at(0)?, f32_at(1)?, f32_at(2)?, f32_at(3)?]),
                16,
            ),
            0x1900 => (Value::Guid(guid_at(b, p)?), 16),
            0x1700 => return self.string(p, field),
            0x1f00 => return self.variant(p, field),
            0x4000 | 0x4200 => return self.pointer(p, field),
            0x4300 | 0x4400 => return self.asset_ref(p, field),
            _ => return None,
        };
        self.push(p..p + width, Record::Value(value), Some(field));
        Some(p + width)
    }

    fn string(&mut self, p: usize, field: Field) -> Option<usize> {
        let (text, end) = read_str(self.b, p)?;
        // A `XxxCrc` u32 right before its string becomes one record, so text
        // edits keep the pair in sync.
        let mut start = p;
        let mut with_crc = false;
        if let Some(last) = self.spans.last()
            && let Record::Value(Value::U32(c)) = last.record
            && last.range.end == p
            && last
                .field
                .is_some_and(|f| f.object == field.object && f.prop + 1 == field.prop)
            && !text.is_empty()
            && c == crc32(text.as_bytes())
        {
            start = last.range.start;
            with_crc = true;
            self.spans.pop();
        }
        self.push(
            start..end,
            Record::Str(StrRecord { with_crc, text }),
            Some(field),
        );
        Some(end)
    }

    // `00 <tag> <payload>`; tag `40` is `<class crc>` plus a pointer.
    fn variant(&mut self, p: usize, field: Field) -> Option<usize> {
        let b = self.b;
        if *b.get(p)? != 0 {
            return None;
        }
        let q = p + 2;
        let (value, end) = match *b.get(p + 1)? {
            0x00 => (DefaultValue::None, q),
            0x01 => (DefaultValue::Bool(*b.get(q)?), q + 1),
            0x07 => (DefaultValue::S32(u32_at(b, q)? as i32), q + 4),
            0x08 => (DefaultValue::U32(u32_at(b, q)?), q + 4),
            0x10 => (DefaultValue::F32(u32_at(b, q)?), q + 4),
            t @ 0x11..=0x13 => {
                let n = (t - 0x0f) as usize;
                let v: Option<Vec<u32>> = (0..n).map(|i| u32_at(b, q + 4 * i)).collect();
                (DefaultValue::Vec(v?), q + 4 * n)
            }
            0x17 => {
                let (text, end) = read_str(b, q)?;
                (DefaultValue::Str(text), end)
            }
            0x19 => (DefaultValue::ObjectId(guid_at(b, q)?), q + 16),
            0x40 => {
                let class = u32_at(b, q)?;
                self.push(
                    p..q + 4,
                    Record::Default(DefaultValue::Object(class)),
                    Some(field),
                );
                return self.pointer(q + 4, field);
            }
            _ => return None,
        };
        self.push(p..end, Record::Default(value), Some(field));
        Some(end)
    }

    // `00` null, `01 <class> <id>` reference, `02 <id form> …` owned object.
    fn pointer(&mut self, p: usize, field: Field) -> Option<usize> {
        let b = self.b;
        match *b.get(p)? {
            2 => match *b.get(p + 1)? {
                0 => self.object_at(p, p + 2, ObjectId::None, Some(field)),
                2 => self.object_at(p, p + 18, ObjectId::Id(guid_at(b, p + 2)?), Some(field)),
                _ => None,
            },
            _ => self.asset_ref(p, field),
        }
    }

    fn asset_ref(&mut self, p: usize, field: Field) -> Option<usize> {
        let b = self.b;
        match *b.get(p)? {
            0 => {
                self.push(p..p + 1, Record::Null, Some(field));
                Some(p + 1)
            }
            1 => {
                let r = Reference {
                    class: u32_at(b, p + 1)?,
                    id: guid_at(b, p + 5)?,
                };
                self.push(p..p + 21, Record::Ref(r), Some(field));
                Some(p + 21)
            }
            _ => None,
        }
    }

    // Collects a CustomPropertyInstance's decoded parts from the spans its
    // values just produced.
    fn custom_property(&mut self, obj: usize, props: &[&Prop]) {
        let (mut uid, mut kind, mut default, mut name) = (None, None, None, None);
        let (mut object_asset, mut enum_asset) = (None, None);
        for (k, s) in self.spans[obj + 1..].iter().enumerate() {
            let Some(f) = s.field.filter(|f| f.object == obj) else {
                continue;
            };
            let idx = obj + 1 + k;
            match (props[f.prop as usize - 1].name_crc, &s.record) {
                (PROP_UNIQUE_ID, Record::Value(Value::U32(v))) => uid = Some(*v),
                (PROP_PROPERTY_TYPE, Record::Value(Value::Enum { value, .. })) => {
                    kind = Some(*value)
                }
                (PROP_DEFAULT_VALUE, Record::Default(_)) if default.is_none() => {
                    default = Some(idx)
                }
                (PROP_OBJECT_TEMPLATE, Record::Ref(r)) => object_asset = Some(*r),
                (PROP_ENUM_ASSET, Record::Ref(r)) => enum_asset = Some(*r),
                (PROP_NAME, Record::Str(_)) => name = Some(idx),
                _ => {}
            }
        }
        if let (Some(uid), Some(kind), Some(default), Some(name)) = (uid, kind, default, name) {
            self.properties.push(CustomProperty {
                object: obj,
                uid,
                kind,
                default,
                object_asset,
                enum_asset,
                name,
            });
        }
    }
}

// Fallback for assets that use a class the schema table lacks: heap objects
// are anchored through the layout tokens, and between them the scan claims
// what proves itself (crc-checked strings, references, embedded objects,
// colors); everything else stays opaque.
struct Body<'a> {
    b: &'a [u8],
    types: &'a [TypeEntry],
    classes: HashSet<u32>,
    // Classes a `01 <class> <id>` reference may name outside a custom
    // property: the file's class table (local ids) plus every kind root class
    // (other assets).
    ref_classes: HashSet<u32>,
    spans: Vec<Span>,
}

impl<'a> Body<'a> {
    fn new(b: &'a [u8], types: &'a [TypeEntry]) -> Body<'a> {
        let classes: HashSet<u32> = types.iter().map(|t| t.class).collect();
        let mut ref_classes = classes.clone();
        ref_classes.extend(KINDS.iter().map(|k| k.2));
        Body {
            b,
            types,
            classes,
            ref_classes,
            spans: Vec::new(),
        }
    }

    fn push(&mut self, range: Range<usize>, record: Record) {
        self.spans.push(Span::new(range, record));
    }

    fn parse(&mut self, start: usize, layout: &Layout) -> Result<()> {
        // Heap objects: one per 0x40 token, in the same pre-order as the body.
        let mut anchors: Vec<(usize, usize, ObjectHeader)> = Vec::new();
        let mut cursor = start;
        for (k, tok) in layout
            .tokens
            .iter()
            .filter(|t| t.kind == TOKEN_OBJECT)
            .enumerate()
        {
            let class = self.types[tok.class_index.unwrap() as usize].class;
            let found = if k == 0 {
                self.root_at(cursor, class)
            } else {
                self.find_heap(cursor, class)
            };
            let (s, e, header) = found.with_context(|| {
                format!("heap object {k} (class {class:#010x}) not found after {cursor:#x}")
            })?;
            anchors.push((s, e, header));
            cursor = e;
        }
        ensure!(!anchors.is_empty(), "layout has no root object");

        let mut pos = start;
        for i in 0..anchors.len() {
            let (s, e) = (anchors[i].0, anchors[i].1);
            let limit = anchors.get(i + 1).map_or(self.b.len(), |a| a.0);
            self.scan(pos, s);
            let header = anchors[i].2.clone();
            pos = self.object_values(s, e, header, limit);
            self.scan(pos, limit);
            pos = limit;
        }
        Ok(())
    }

    // The root sits exactly at the body start: `02 <file guid> <root class>`.
    fn root_at(&self, p: usize, class: u32) -> Option<(usize, usize, ObjectHeader)> {
        let b = self.b;
        if *b.get(p)? != 2 || u32_at(b, p + 17)? != class {
            return None;
        }
        let id = ObjectId::Root(guid_at(b, p + 1)?);
        let (prop_count, mask, end) = read_counts(b, p + 21)?;
        Some((
            p,
            end,
            ObjectHeader {
                id,
                class,
                prop_count,
                mask,
                heap: true,
            },
        ))
    }

    // Next `02 00 <class>` or `02 02 <id> <class>` header at or after `from`.
    fn find_heap(&self, from: usize, class: u32) -> Option<(usize, usize, ObjectHeader)> {
        let b = self.b;
        let needle = class.to_le_bytes();
        (from + 2..b.len().saturating_sub(3))
            .filter(|&c| b[c..c + 4] == needle)
            .find_map(|c| self.header_ending_at(from, c, true))
    }

    // Object header whose class crc sits at `c`, starting no earlier than `from`.
    fn header_ending_at(
        &self,
        from: usize,
        c: usize,
        heap: bool,
    ) -> Option<(usize, usize, ObjectHeader)> {
        let b = self.b;
        let class = u32_at(b, c)?;
        let (start, id) = if c >= from + 2 && b[c - 2] == 2 && b[c - 1] == 0 {
            (c - 2, ObjectId::None)
        } else if c >= from + 18 && b[c - 18] == 2 && b[c - 17] == 2 {
            (c - 18, ObjectId::Id(guid_at(b, c - 16)?))
        } else {
            return None;
        };
        let (prop_count, mask, end) = read_counts(b, c + 4)?;
        Some((
            start,
            end,
            ObjectHeader {
                id,
                class,
                prop_count,
                mask,
                heap,
            },
        ))
    }

    // Emits the header span and whatever values we can decode for its class.
    // Returns where generic scanning resumes.
    fn object_values(&mut self, s: usize, e: usize, header: ObjectHeader, limit: usize) -> usize {
        let is_color = header.class == CLASS_COLOR
            && header.prop_count == COLOR_PROPS
            && *header.mask == [COLOR_MASK];
        self.push(s..e, Record::Object(header));
        if is_color && e + 3 <= limit {
            self.push(
                e..e + 3,
                Record::Color(self.b[e..e + 3].try_into().unwrap()),
            );
            return e + 3;
        }
        e
    }

    fn crc_string(&self, p: usize, limit: usize) -> Option<(usize, StrRecord)> {
        let b = &self.b[..limit];
        let crc = u32_at(b, p)?;
        let (text, end) = read_str(b, p + 4)?;
        if crc32(text.as_bytes()) != crc || end > p + 7 + text.len() {
            return None;
        }
        valid_crc_text(&text).then_some((
            end,
            StrRecord {
                with_crc: true,
                text,
            },
        ))
    }

    fn bare_string(&self, p: usize, limit: usize) -> Option<(usize, StrRecord)> {
        let b = &self.b[..limit];
        let (n, q) = read_varint(b, p)?;
        if !(4..=MAX_SCAN_STR as u32).contains(&n) || q > p + 2 || *b.get(q)? != 1 {
            return None;
        }
        let end = q + 1 + n as usize;
        let text = std::str::from_utf8(b.get(q + 1..end)?).ok()?;
        valid_bare_text(text).then(|| {
            (
                end,
                StrRecord {
                    with_crc: false,
                    text: text.into(),
                },
            )
        })
    }

    // Claims what can be proven between `from` and `to`; the rest is opaque.
    fn scan(&mut self, from: usize, to: usize) {
        let mut p = from;
        let mut opaque = from;
        while p < to {
            let mark = self.spans.len();
            if let Some(end) = self.scan_one(p, to) {
                // scan_one pushed its spans; the pending opaque run goes first.
                if opaque < p {
                    self.spans
                        .insert(mark, Span::new(opaque..p, Record::Opaque));
                }
                p = end;
                opaque = end;
            } else {
                self.spans.truncate(mark);
                p += 1;
            }
        }
        if opaque < to {
            self.push(opaque..to, Record::Opaque);
        }
    }

    fn scan_one(&mut self, p: usize, to: usize) -> Option<usize> {
        let b = self.b;
        if let Some((end, rec)) = self.crc_string(p, to) {
            self.push(p..end, Record::Str(rec));
            return Some(end);
        }
        if b[p] == 2 {
            for c in [p + 2, p + 18] {
                let Some(class) = u32_at(&b[..to], c) else {
                    continue;
                };
                if !self.classes.contains(&class) {
                    continue;
                }
                if let Some((s, e, header)) = self.header_ending_at(p, c, false) {
                    // Embedded objects in the shipped files never set mask bit
                    // 0 and always have some property present.
                    if s == p && e <= to && header.mask[0] & 0x80 == 0 && header.present_count() > 0
                    {
                        return Some(self.object_values(s, e, header, to));
                    }
                }
            }
        }
        if b[p] == 1
            && p + 21 <= to
            && let Some(class) = u32_at(b, p + 1).filter(|c| self.ref_classes.contains(c))
        {
            let r = Reference {
                class,
                id: guid_at(b, p + 5)?,
            };
            self.push(p..p + 21, Record::Ref(r));
            return Some(p + 21);
        }
        if let Some((end, rec)) = self.bare_string(p, to) {
            self.push(p..end, Record::Str(rec));
            return Some(end);
        }
        None
    }
}

// ---------------------------------------------------------------------------
// Directory summary (the `phx.py objects` equivalent)

#[derive(Debug, Clone, Default)]
pub struct KindSummary {
    pub files: usize,
    pub bytes: usize,
    pub objects: usize,
    pub strings: usize,
    pub references: usize,
    pub properties: usize,
    pub opaque_bytes: usize,
    /// Object headers whose class crc resolves to a name.
    pub classes_named: usize,
    /// Files whose encode is byte-identical to the input.
    pub roundtrip: usize,
}

#[derive(Debug, Clone, Default)]
pub struct Summary {
    /// Keyed by the two kind letters, e.g. `WI`.
    pub kinds: BTreeMap<String, KindSummary>,
    pub failed: Vec<(PathBuf, String)>,
}

/// Parses every `PHX*` file directly in `dir`; other files are skipped.
pub fn summarize(dir: &Path, names: &Names) -> Result<Summary> {
    let mut summary = Summary::default();
    let mut paths: Vec<PathBuf> = std::fs::read_dir(dir)
        .with_context(|| format!("reading {}", dir.display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.is_file())
        .collect();
    paths.sort();
    for path in paths {
        let bytes = std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
        if !bytes.starts_with(b"PHX") {
            continue;
        }
        let asset = match Asset::parse(bytes) {
            Ok(a) => a,
            Err(e) => {
                summary.failed.push((path, format!("{e:#}")));
                continue;
            }
        };
        let k = summary.kinds.entry(asset.header.kind_str()).or_default();
        k.files += 1;
        k.bytes += asset.bytes.len();
        for (_, o) in asset.objects() {
            k.objects += 1;
            k.classes_named += usize::from(asset.name(names, o.class).is_some());
        }
        k.strings += asset.strings().count();
        k.references += asset.references().count();
        k.properties += asset.properties.len();
        k.opaque_bytes += asset
            .spans
            .iter()
            .filter(|s| s.record == Record::Opaque)
            .map(|s| s.range.len())
            .sum::<usize>();
        k.roundtrip += usize::from(asset.encode() == asset.bytes);
    }
    Ok(summary)
}

#[cfg(test)]
mod tests;
