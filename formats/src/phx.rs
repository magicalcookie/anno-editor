// Compiled Phoenix UI assets (`data/ui/studio/generated/<guid>`, magic
// `PHXxx`): byte-exact span decode, re-encode, and the few edits the UI editor
// offers.
//
// The file is a schema-driven object stream. The 16-byte header, the class
// table and the heap section decode fully. The body is objects
// (`02 <id form> <class crc> <prop count> <presence mask>`) followed by their
// property values in class order. The per-class property lists come from the
// exes' runtime registries (formats/data/<game>/classes.tsv, read by
// phx/tables.rs), picked per class by the schema hash in the file's class
// table. Assets that use a class or version the tables lack fall back to a
// scan that claims strings, references and colors and keeps the rest opaque.
//
// Widget trees, components, geometry and image lookup live in phx/widget.rs.
// Evaluating a screen's layout and bindings is the shell's (src/ui/layout.rs,
// src/ui/evaluate.rs).

pub mod animation;
pub mod binding;
mod pwl;
pub use pwl::{EnumEntry, EnumInfo, SourceDirs, Sources, TemplateInfo};

mod tables;
pub use tables::DataFields;
mod widget;

pub use widget::{
    Component, Geometry, ImageInfo, LabelText, Palettes, PropValue, Removed, TextStyle, Widget,
    WidgetCopy, WidgetImage, font_file, image_info, localization_line_id, localized_font,
    parse_texts, text_style,
};

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt;
use std::ops::Range;
use std::path::Path;

use anyhow::{Context, Result, bail, ensure};

use crate::varint;

// Every record: a name or class crc is a u32, an id 16 bytes.
const CRC: usize = size_of::<u32>();
const ID: usize = size_of::<Guid>();

// Scanner policy. Longest real string is 232 bytes. Bare (crc-less) string
// candidates past this, or shorter than the minimum, are noise in float
// runs, not text; so are lengths whose varint takes more than two bytes.
const MAX_SCAN_STR: usize = 4096;
const MIN_SCAN_STR: u32 = 4;
const MAX_SCAN_LEN_VARINT: usize = 2;
// Classes per file top out around 60; the varint is untrusted input.
const MAX_TYPES: u32 = 4096;
const MAX_ALLOCATIONS: u32 = 1 << 22;

fn crc32(bytes: &[u8]) -> u32 {
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
    /// Wire byte `k` holds byte `WIRE_ORDER[k]` of the text form: the first
    /// three groups little-endian, the rest as written. The order is its own
    /// inverse, so it maps text to wire and back.
    const WIRE_ORDER: [usize; 16] = [3, 2, 1, 0, 5, 4, 7, 6, 8, 9, 10, 11, 12, 13, 14, 15];
    /// Where the text form's dashes go: before these bytes.
    const DASHES: [usize; 4] = [4, 6, 8, 10];

    /// `text`, the 16 bytes in text order, in wire order.
    const fn from_text_order(text: [u8; 16]) -> Guid {
        let mut out = [0u8; 16];
        let mut k = 0;
        while k < 16 {
            out[k] = text[Guid::WIRE_ORDER[k]];
            k += 1;
        }
        Guid(out)
    }

    /// A new version-4 style id. Seeded from the std hasher's per-process
    /// random keys, a clock and a counter; ids only need to be unique.
    fn random() -> Guid {
        use std::hash::{BuildHasher, Hasher};
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos() as u64);
        let mut b = [0u8; 16];
        for (k, chunk) in b.chunks_mut(size_of::<u64>()).enumerate() {
            let mut h = std::collections::hash_map::RandomState::new().build_hasher();
            h.write_u64(now);
            h.write_u64(n);
            h.write_usize(k);
            chunk.copy_from_slice(&h.finish().to_le_bytes());
        }
        // In text order, the version is the high nibble of byte 6 and the
        // variant the top bits of byte 8.
        b[6] = (b[6] & 0x0f) | 0x40;
        b[8] = (b[8] & 0x3f) | 0x80;
        Guid::from_text_order(b)
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
        Guid::from_text_order(g)
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
        Some(Guid::from_text_order(g))
    }
}

impl fmt::Display for Guid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (k, &at) in Guid::WIRE_ORDER.iter().enumerate() {
            if Guid::DASHES.contains(&k) {
                f.write_str("-")?;
            }
            write!(f, "{:02x}", self.0[at])?;
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
pub struct Header(pub [u8; Header::LEN]);

impl Header {
    const LEN: usize = 0x10;

    /// The 21 asset kinds of the runtime kind table:
    /// magic letters, kind id, crc32 of the root class.
    const KINDS: [([u8; 2], u8, u32); 21] = [
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

    /// Bytes 0..3; the kind letters (`Header::KINDS`) follow.
    const MAGIC: [u8; 3] = *b"PHX";

    pub fn kind_str(&self) -> String {
        String::from_utf8_lossy(&self.0[3..5]).into_owned()
    }
}

/// One entry of the class table at 0x10. Entry 0 is the root class; its
/// schema hash is what the earlier notes called the per-kind "schema guid".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypeEntry {
    class: u32,
    /// Per-class schema hash, byte-identical to the runtime registry block.
    schema: [u8; 16],
    /// Heap instances of this class in the file (= its `0x40` allocations).
    /// Value-embedded structs such as `Color` count 0 even when present.
    instances: u32,
}

/// The heap section between the class table and the root object: every heap
/// object and non-empty array the body holds, in pre-order (the rule is on
/// `Asset::rebuild`). `counts` holds the per-kind sums of `n` for every kind
/// but heap objects.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Heap {
    counts: Vec<(AllocKind, u32)>,
    /// Always 0 in the shipped files.
    reserved: u8,
    pub allocations: Vec<Allocation>,
}

/// One entry of the heap section: a heap object of a class, or a run of
/// `count` array elements of one kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Allocation {
    kind: AllocKind,
    count: u32,
    /// Index into the class table, for `0x40` allocations only.
    class_index: Option<u32>,
}

/// An allocation's kind byte: `<kind> <varint n> [<varint class index>]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct AllocKind(u8);

impl AllocKind {
    /// One heap object of the class it names (n is always 1).
    const OBJECT: AllocKind = AllocKind(WireType::Pointer as u8);
    /// A binding's source and target index arrays, after its targets' run
    /// (see [`Asset::rebuild`]).
    const BINDING_INDICES: AllocKind = AllocKind(0x51);

    /// A run of n array elements of wire type `t`: the root's child list
    /// is `OwnedPointer`'s, u32 arrays `U32`'s.
    const fn slots(t: WireType) -> AllocKind {
        AllocKind(t as u8)
    }
}

/// Replace bytes `range` of the file (inside the body) with `bytes`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Splice {
    range: Range<usize>,
    bytes: Vec<u8>,
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

impl ObjectId {
    /// A pointer slot's first byte, any pointer or asset reference type:
    /// null, a [`Reference`], or an owned object.
    const NULL: u8 = 0x00;
    const REFERENCE: u8 = 0x01;
    const OWNED: u8 = 0x02;
    /// An owned object's id form after `OWNED`; the root carries its guid
    /// bare instead.
    const NO_ID: u8 = 0x00;
    const WITH_ID: u8 = 0x02;
    /// A pointer tag or an id form: one byte each.
    const TAG: usize = size_of::<u8>();

    fn encode(&self, out: &mut Vec<u8>) {
        out.push(ObjectId::OWNED);
        match self {
            ObjectId::Root(g) => out.extend_from_slice(&g.0),
            ObjectId::None => out.push(ObjectId::NO_ID),
            ObjectId::Id(g) => {
                out.push(ObjectId::WITH_ID);
                out.extend_from_slice(&g.0);
            }
        }
    }

    /// Bytes before the class crc.
    fn len(&self) -> usize {
        match self {
            ObjectId::Root(_) => ObjectId::TAG + ID,
            ObjectId::None => 2 * ObjectId::TAG,
            ObjectId::Id(_) => 2 * ObjectId::TAG + ID,
        }
    }
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
    mask: Box<[u8]>,
    /// Located through a `0x40` allocation (heap object) rather than by scan.
    heap: bool,
}

impl ObjectHeader {
    /// Where presence bit `prop` sits: its mask byte, and the bit in it,
    /// most significant first.
    fn mask_bit(prop: usize) -> (usize, u8) {
        (prop / 8, 0x80 >> (prop % 8))
    }

    fn bit(&self, prop: usize) -> bool {
        let (byte, bit) = ObjectHeader::mask_bit(prop);
        self.mask.get(byte).is_some_and(|m| m & bit != 0)
    }

    fn present(&self, prop: usize) -> bool {
        prop < self.prop_count as usize && self.bit(prop)
    }
    fn present_count(&self) -> u32 {
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
    /// The encoding byte after a non-empty string's length: ASCII, or text
    /// with non-ASCII UTF-8.
    const ASCII: u8 = 0x01;
    const UTF8: u8 = 0x03;

    fn crc(&self) -> u32 {
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
    fn wire_type(&self) -> WireType {
        match self {
            DefaultValue::None => WireType::None,
            DefaultValue::Bool(_) => WireType::Bool,
            DefaultValue::S32(_) => WireType::S32,
            DefaultValue::U32(_) => WireType::U32,
            DefaultValue::F32(_) => WireType::F32,
            DefaultValue::Vec(v) => WireType::vector(v.len()).expect("a 2- to 4-lane vector"),
            DefaultValue::Str(_) => WireType::Str,
            DefaultValue::ObjectId(_) => WireType::Id,
            DefaultValue::Object(_) => WireType::Pointer,
        }
    }

    fn encode(&self, out: &mut Vec<u8>) {
        out.push(0);
        out.push(self.wire_type() as u8);
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
    /// The heap section, see [`Asset::heap`].
    Heap,
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
/// (1-based, the presence-mask bit; see `wire_props`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Field {
    pub object: usize,
    prop: u16,
}

/// One class version: the exe's current one, or an older one found in
/// installed assets (`phx/tables.rs`).
#[derive(Debug)]
struct Class {
    crc: u32,
    name: Option<&'static str>,
    /// Base class crc, 0 for the registry root.
    base: u32,
    /// Schema hash of this version; a file's class table carries the hash
    /// of the version it was written with.
    hash: [u8; 16],
    /// The version's own descriptor rows: a current version's in registry
    /// order, an older one's serialized rows in wire order.
    props: &'static [Prop],
}

/// A wire type, as one byte: the high byte of a registry type word, the tag
/// of a variant's payload, and the kind of an allocation that counts array
/// slots of that type. The codec reads and writes types through this table
/// alone; a byte it lacks (`0x1a`, `0x41`… in a few registry rows) leaves
/// the asset to the scan.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
enum WireType {
    /// A variant with no payload (events, actions, object references).
    None = 0x00,
    /// One byte: 0 and 1 read as a bool, any other value is kept as a byte.
    Bool = 0x01,
    U8 = 0x04,
    U16 = 0x06,
    S32 = 0x07,
    U32 = 0x08,
    U64 = 0x0a,
    /// A u32 entry of the enum type the prop's `type_crc` names.
    Enum = 0x0b,
    F32 = 0x10,
    Vec2 = 0x11,
    Vec3 = 0x12,
    Vec4 = 0x13,
    /// `<varint len> <encoding> <bytes>`.
    Str = 0x17,
    /// A 16-byte object id.
    Id = 0x19,
    /// `00 <type> <payload>`: a CustomPropertyInstance default.
    Variant = 0x1f,
    /// Two pointer kinds the exe tells apart and the wire does not: null, a
    /// reference, or an owned object (see [`ObjectId`]). Owned children
    /// (Components, Effects, CustomProperties) are `OwnedPointer`.
    Pointer = 0x40,
    OwnedPointer = 0x42,
    /// Two asset reference kinds, alike on the wire: null or a reference.
    AssetRef = 0x43,
    AssetRef2 = 0x44,
}

impl WireType {
    const ALL: [WireType; 19] = [
        WireType::None,
        WireType::Bool,
        WireType::U8,
        WireType::U16,
        WireType::S32,
        WireType::U32,
        WireType::U64,
        WireType::Enum,
        WireType::F32,
        WireType::Vec2,
        WireType::Vec3,
        WireType::Vec4,
        WireType::Str,
        WireType::Id,
        WireType::Variant,
        WireType::Pointer,
        WireType::OwnedPointer,
        WireType::AssetRef,
        WireType::AssetRef2,
    ];

    fn of(byte: u8) -> Option<WireType> {
        WireType::ALL.into_iter().find(|t| *t as u8 == byte)
    }

    /// The bytes of a fixed-width value; None for the rest.
    fn width(self) -> Option<usize> {
        Some(match self {
            WireType::Bool | WireType::U8 => 1,
            WireType::U16 => 2,
            WireType::S32 | WireType::U32 | WireType::Enum | WireType::F32 => 4,
            WireType::U64 => 8,
            WireType::Vec2 | WireType::Vec3 | WireType::Vec4 => 4 * self.lanes()?,
            WireType::Id => 16,
            _ => return None,
        })
    }

    /// The f32 lanes of a vector type.
    fn lanes(self) -> Option<usize> {
        match self {
            WireType::Vec2 => Some(2),
            WireType::Vec3 => Some(3),
            WireType::Vec4 => Some(4),
            _ => None,
        }
    }

    /// The vector type of `n` lanes.
    fn vector(n: usize) -> Option<WireType> {
        [WireType::Vec2, WireType::Vec3, WireType::Vec4]
            .into_iter()
            .find(|t| t.lanes() == Some(n))
    }
}

/// One property descriptor row `{type word, value type crc, mode, name crc}`;
/// the registry's struct offset is checked but not kept.
#[derive(Debug, Clone)]
struct Prop {
    name_crc: u32,
    name: Option<&'static str>,
    /// Type word: the high byte is the [`WireType`], the low bit marks an
    /// array (`Prop::wire_type`, `Prop::is_array`).
    ty: u16,
    /// Value type: enum type, pointee class or asset class crc; 0 for scalars.
    type_crc: u32,
    mode: u32,
}

impl Prop {
    /// The type word's array bit.
    const ARRAY: u16 = 1;
    /// The mode bits that put a prop on the wire (see [`Prop::serialized`]).
    const SERIALIZED: u32 = 0x1_0001;

    /// The type word's high byte: a [`WireType`] when the codec knows it.
    fn type_byte(&self) -> u8 {
        (self.ty >> 8) as u8
    }

    fn wire_type(&self) -> Option<WireType> {
        WireType::of(self.type_byte())
    }

    fn is_array(&self) -> bool {
        self.ty & Prop::ARRAY != 0
    }

    /// Mode bit 0 (`0x2001`, `0x2009`) or bit 16 (`0x12000`) = written to the
    /// wire; `0x2000`/`0x2080` (transient) and the computed accessors
    /// (`0x1c`, `0x94`) are not.
    fn serialized(&self) -> bool {
        self.mode & Prop::SERIALIZED != 0
    }
}

// Classes the codec reads by name: `name_crc` of the name, or the bare crc
// where the name is unknown.
impl Class {
    // Color: 4 properties, 1..=3 present (R, G, B), one byte each. Seen
    // 6344 times, no variant.
    const COLOR_PROPS: u8 = 4;
    const COLOR_MASK: u8 = 0x70;
    const COLOR_BYTES: usize = 3;

    /// Classes stored by value: never counted in the class table, never a
    /// allocation. The same three in all 4,682 installed assets; every other
    /// class is a heap object wherever it occurs.
    const BY_VALUE: [u32; 3] = [Class::COLOR_TYPE, Class::COLOR, 0xc761_3662];

    /// The CustomPropertyInstance class.
    const CUSTOM_PROPERTY: u32 = name_crc(b"CustomPropertyInstance");
    /// Color: three RGB bytes, no alpha on the wire.
    const COLOR: u32 = name_crc(b"Color");
    /// ColorType: optional palette reference, palette entry, `Color`.
    const COLOR_TYPE: u32 = name_crc(b"ColorType");
    const OBJECT: u32 = name_crc(b"Object");
    const GROUP_TEMPLATE: u32 = name_crc(b"GroupTemplate");
    const GROUP_INSTANCE: u32 = name_crc(b"GroupInstance");
    const WIDGET_INSTANCE: u32 = name_crc(b"WidgetInstance");
    const IMAGE_ASSET: u32 = name_crc(b"ImageAsset");
    // TextStyle root; the PHXTS kind root 0xc697c96c is its abstract base.
    const TEXT_STYLE: u32 = name_crc(b"SlugTextStyleAsset");
    const CONTAINER_TEMPLATE: u32 = name_crc(b"ContainerWidgetTemplate");
    const CONTAINER_INSTANCE: u32 = name_crc(b"ContainerWidgetInstance");
    const IMAGE_WIDGET: u32 = name_crc(b"ImageWidgetInstance");
    const LABEL_WIDGET: u32 = name_crc(b"LabelWidgetInstance");
    const PALETTE_COLOR: u32 = name_crc(b"ColorPaletteEntry");
    const FONT_ASSET: u32 = name_crc(b"FontBaseAsset");
    const COMPONENT_INSTANCE: u32 = name_crc(b"ComponentInstance");
    const PROPERTY_VALUE: u32 = name_crc(b"PropertyValue");
    const EFFECT_INSTANCE: u32 = name_crc(b"EffectInstance");
    const PROPERTY_CONTAINER: u32 = name_crc(b"PropertyContainer");
    const PROPERTY_ID: u32 = name_crc(b"PropertyId");
    const PROVIDER: u32 = name_crc(b"DataProviderContainer");
    const STEP: u32 = name_crc(b"DataProviderId");
    const TEMPLATE_CONTAINER: u32 = name_crc(b"CustomPropertyContainer");
    const CONSTANT: u32 = name_crc(b"ConstantPropertyContainer");
    const BINDING: u32 = 0xedcf_0433;
    /// The class widget references in provider steps name.
    const WIDGET_REF: u32 = 0xbb96_607d;
    const CLIP: u32 = name_crc(b"AnimationInstance");
}

// Properties the codec reads by name, as [`Class`]'s.
impl Prop {
    // A binding's source and target index arrays (class `0xedcf0433`).
    const BINDING_SOURCES: u32 = 0x28c3_8074;
    const BINDING_TARGETS: u32 = 0xcec4_084b;
    const UNIQUE_ID: u32 = name_crc(b"UniqueId");
    const PROPERTY_TYPE: u32 = name_crc(b"PropertyType");
    const DEFAULT_VALUE: u32 = name_crc(b"DefaultValue");
    const OBJECT_TEMPLATE: u32 = name_crc(b"ObjectTemplate");
    const ENUM_ASSET: u32 = name_crc(b"EnumAsset");
    // The name string of named objects (widgets, custom properties); its
    // `NameCrc` u32 sits right before it.
    const NAME: u32 = name_crc(b"PhoenixDebugName");
    // Property name crcs.
    const POSITION: u32 = name_crc(b"Position");
    const PIVOT: u32 = name_crc(b"PivotPoint");
    const ROTATION: u32 = name_crc(b"RotationAngle");
    const SCALE: u32 = name_crc(b"Scale");
    const ALPHA_TINT: u32 = name_crc(b"AlphaTint");
    const COLOR_TINT: u32 = name_crc(b"ColorTint");
    const VISIBLE: u32 = name_crc(b"Visible");
    const IS_HIDDEN: u32 = name_crc(b"IsHidden");
    const ANCHOR: u32 = name_crc(b"AnchorPoint");
    // Anchor metrics x/y (unnamed; registered as legacy aliases),
    // then WidthResizing/HeightResizing.
    const ANCHOR_METRICS: [u32; 2] = [0x5c63_f4bb, 0xc16c_15cd];
    const RESIZE_METRICS: [u32; 2] = [name_crc(b"WidthResizing"), name_crc(b"HeightResizing")];
    const ASPECT_RATIO: u32 = name_crc(b"AspectRatio");
    const Z_INDEX: u32 = name_crc(b"ZIndex");
    const ENUM_ENTRY: u32 = name_crc(b"EnumEntry");
    const WIDGET_TYPE: u32 = name_crc(b"WidgetType");
    const PARENT_WIDGET_TYPE: u32 = name_crc(b"ParentWidgetType");
    const CURRENT_ENUM_VALUE: u32 = name_crc(b"CurrentEnumValue");
    const CONSIDER_SIZE: u32 = name_crc(b"ConsiderSize");
    const WIDGET_INSTANCES: u32 = name_crc(b"WidgetInstances");
    const GROUP_TEMPLATE: u32 = name_crc(b"GroupTemplate");
    const IMAGE_ID: u32 = name_crc(b"ImageId");
    const SIZE: u32 = name_crc(b"Size");
    // Unnamed ImageAsset field holding the UV rectangle object.
    const UV_RECT: u32 = 0x65fc_c08e;
    const TEXTURE_FILTERING: u32 = name_crc(b"TextureFiltering");
    const IMAGE_ATLAS: u32 = name_crc(b"ImageAtlas");
    const TOP_LEFT: u32 = name_crc(b"TopLeft");
    const BOTTOM_RIGHT: u32 = name_crc(b"BottomRight");
    const IMAGE: u32 = name_crc(b"Image");
    const UV_OFFSET: u32 = name_crc(b"UVOffset");
    const UV_SCALE: u32 = name_crc(b"UVScale");
    const UV_ROTATION: u32 = name_crc(b"UVRotation");
    const FILL_MODE: u32 = name_crc(b"FillMode");
    /// See [`WidgetImage::path`]; its name is unknown.
    const IMAGE_PATH: u32 = name_crc(b"DynamicImageId");
    const TEXT: u32 = name_crc(b"Text");
    const DISPLAYED_TEXT: u32 = name_crc(b"DisplayedText");
    const LOCALIZATION_ID: u32 = name_crc(b"LocalizationId");
    // Unnamed; self-typed with the PHXLG kind root class.
    const FONT: u32 = 0xe152_3167;
    const TEXT_STYLE: u32 = name_crc(b"TextStyle");
    const ALIGNMENT: [u32; 2] = [name_crc(b"AlignmentX"), name_crc(b"AlignmentY")];
    const TEXT_PLANE: u32 = name_crc(b"TextPlane");
    const OVERFLOW: [u32; 2] = [
        name_crc(b"HorizontalOverflow"),
        name_crc(b"VerticalOverflow"),
    ];
    const SIZING: [u32; 2] = [name_crc(b"HorizontalSizing"), name_crc(b"VerticalSizing")];
    const MAX_LINES: u32 = name_crc(b"MaxLineCount");
    const LINE_SPACING: u32 = name_crc(b"LineSpacing");
    const LOCALIZED_FONT: u32 = name_crc(b"LocalizedFont");
    const BASE_COLOR: u32 = name_crc(b"BaseColor");
    const OUTLINE_ENABLED: u32 = name_crc(b"OutlineEnabled");
    // Unnamed ColorType right after OutlineEnabled.
    const OUTLINE_COLOR: u32 = name_crc(b"OutlineColor");
    const OUTLINE_ALPHA: u32 = name_crc(b"OutlineAlpha");
    const SHADOW_ENABLED: u32 = name_crc(b"ShadowEnabled");
    const SHADOW_COLOR: u32 = name_crc(b"ShadowColor");
    const SHADOW_ALPHA: u32 = name_crc(b"ShadowAlpha");
    const SHADOW_DISTANCE: u32 = name_crc(b"ShadowDistance");
    const SHADOW_ANGLE: u32 = name_crc(b"ShadowAngle");
    const LETTER_SPACING: u32 = name_crc(b"LetterSpacing");
    const REFERENCE_SIZE: u32 = name_crc(b"ReferenceSize");
    const TEMPLATE: u32 = name_crc(b"Template");
    const GROUPS: u32 = name_crc(b"Groups");
    const PALETTE_COLORS: u32 = name_crc(b"Colors");
    const PALETTE_ENTRY: u32 = 0x46a1_7a33;
    const COMPONENTS: u32 = name_crc(b"Components");
    const PROPERTY_VALUES: u32 = name_crc(b"PropertyValues");
    const COMPONENT_TEMPLATE: u32 = name_crc(b"ComponentTemplate");
    const EFFECTS: u32 = name_crc(b"Effects");
    const EFFECT_TEMPLATE: u32 = name_crc(b"EffectTemplate");
    // PropertyValue: the override's path (u32 ids) and its variant value.
    const PROPERTY_ID_PATH: u32 = name_crc(b"PropertyIdPath");
    const VALUE: u32 = name_crc(b"Value");
    const BINDINGS: u32 = 0xa433_6119;
    /// A binding's converter object; the property shares its class's name.
    const CONVERTER: u32 = name_crc(b"ConverterInstance");
    const CONTAINERS: u32 = name_crc(b"PropertyContainers");
    const TEMPLATE_CONTAINERS: u32 = name_crc(b"CustomPropertyContainers");
    const CONSTANTS: u32 = name_crc(b"ConstantPropertyContainers");
    const PROVIDERS: u32 = name_crc(b"DataProviderContainers");
    const PROPERTY_PATH: u32 = name_crc(b"PropertyPath");
    const FIELD: u32 = name_crc(b"PropertyField");
    const PROVIDER: u32 = name_crc(b"DataProviderIndex");
    const TEMPLATE_INDEX: u32 = name_crc(b"CustomPropertyIndex");
    const STEPS: u32 = name_crc(b"DataProviderPath");
    const TARGET_WIDGET: u32 = name_crc(b"DataProviderObject");
    /// A provider step's DataMode: 0 a widget, 1 the root, 2 a clip.
    const DATA_MODE: u32 = name_crc(b"DataMode");
    const BINDING_MODE: u32 = name_crc(b"BindingMode");
    const BINDING_INFOS: u32 = name_crc(b"BindingInfos");
    const RULE_ENTRY: u32 = name_crc(b"EnumValueId");
    const RULE_TARGET: u32 = name_crc(b"TargetIndex");
    const RULE_ID: u32 = name_crc(b"RuleId");
    const CONVERSION_RULES: u32 = name_crc(b"ConversionRules");
    const CONVERSION_VALUES: u32 = name_crc(b"ConversionValues");
    /// ConversionValue's enum entry UniqueId.
    const CONVERSION_ENTRY: u32 = name_crc(b"EnumEntryUniqueId");
    const USE_COUNT: u32 = name_crc(b"UseCount");
    const PROFILE_ID: u32 = name_crc(b"ProfileID");
    /// Timeline tracks name their slot in the same table.
    const TRACK_TARGET: u32 = 0x1935_a21e;
    const CLIPS: u32 = name_crc(b"AnimationInstances");
    const TRACKS: u32 = name_crc(b"Tracks");
    const KEYS: u32 = name_crc(b"Track");
    const TIME: u32 = name_crc(b"Time");
    const EASE: u32 = 0x8f7a_a617;
    const FRAME_RATE: u32 = name_crc(b"FrameRate");
    const DURATION: u32 = name_crc(b"Duration");
}

fn class_info(crc: u32) -> Option<&'static Class> {
    tables::class(crc)
}

// Base of a class, including the ones the exe dropped (decoded as their base).
fn base_of(crc: u32) -> Option<u32> {
    match class_info(crc) {
        Some(c) => (c.base != 0).then_some(c.base),
        None => tables::removed_base(crc),
    }
}

// Own serialized props of `class` as written with schema `hash`, or in its
// current version when the file names none.
fn own_props(class: u32, hash: Option<&[u8; 16]>) -> Option<Vec<&'static Prop>> {
    let version = match hash {
        Some(h) => tables::class_version(class, h),
        None => class_info(class),
    };
    match version {
        Some(v) => Some(v.props.iter().filter(|p| p.serialized()).collect()),
        // Registry root `Object` and dropped classes have no own props.
        None => (class == Class::OBJECT || tables::removed_base(class).is_some()).then(Vec::new),
    }
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
            match hashes.get(&c).and_then(|h| tables::class_version(c, h)) {
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

/// A class's wire property list in the current (exe) version: base chain
/// first, serialized rows only. Index `i` here is presence-mask bit `i + 1`
/// (bit 0 is never set). Assets written with older versions use
/// [`Asset::class_props`].
fn wire_props(crc: u32) -> Option<&'static [&'static Prop]> {
    static CACHE: std::sync::OnceLock<HashMap<u32, Vec<&'static Prop>>> =
        std::sync::OnceLock::new();
    let cache = CACHE.get_or_init(|| {
        // Every class's current version: the first registry's.
        (tables::registries().flat_map(|r| &r.classes))
            .filter(|&c| std::ptr::eq(c, class_info(c.crc).unwrap()))
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
    object: usize,
    pub uid: u32,
    /// `PropertyType` enum value, see [`property_type_name`].
    pub kind: u32,
    pub default: usize,
    /// Template of object-typed properties (a GameObjectTemplate).
    pub object_asset: Option<Reference>,
    /// The EnumAsset of enum-typed properties.
    pub enum_asset: Option<Reference>,
    name: usize,
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

impl Names {
    // Class and property names with a verified crc. The *Widget*, Group, Track,
    // ImageAtlasAsset and LocalizedFontAsset names were recovered by dictionary
    // search against the class tables; each lands in a coherent family (see
    // phx-decode.md section 3).
    const BUILTIN: &[&str] = &[
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
    const CORPUS: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/data/phx_names.txt"));

    /// The schema's class and property names, the hand-verified list, and the
    /// embedded corpus.
    pub fn builtin() -> Names {
        let mut names = Names::default();
        let schema = (tables::registries().flat_map(|r| &r.classes)).flat_map(|c| {
            c.name
                .into_iter()
                .chain(c.props.iter().filter_map(|p| p.name))
        });
        let corpus = Names::CORPUS.lines().filter(|l| !l.starts_with('#'));
        for n in Names::BUILTIN.iter().copied().chain(schema).chain(corpus) {
            names.insert(n);
        }
        names
    }

    fn insert(&mut self, name: &str) -> u32 {
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

    fn get(&self, crc: u32) -> Option<&str> {
        self.map.get(&crc).map(|s| &**s)
    }
}

// ---------------------------------------------------------------------------
// Asset

#[derive(Debug, Clone)]
pub struct Asset {
    bytes: Vec<u8>,
    header: Header,
    types: Vec<TypeEntry>,
    heap: Heap,
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
    // Its WidgetType chain (`Asset::widget_types`), read on first use.
    widget_types: std::sync::OnceLock<Box<[Guid]>>,
    // See `Asset::widgets`.
    widgets: WidgetCache,
}

/// The palettes an asset's ColorTypes resolve through
/// ([`Asset::use_palettes`]), and [`Asset::widgets`] as last read, keyed by
/// those palettes and their generation.
#[derive(Default)]
struct WidgetCache {
    palettes: std::sync::RwLock<Option<std::sync::Arc<Palettes>>>,
    read: std::sync::Mutex<Option<(PaletteKey, std::sync::Arc<[Widget]>)>>,
}

/// Which palettes (by address) at which of their generations.
type PaletteKey = (usize, u64);

impl Clone for WidgetCache {
    fn clone(&self) -> WidgetCache {
        WidgetCache {
            palettes: std::sync::RwLock::new(self.palettes.read().expect("palettes lock").clone()),
            read: std::sync::Mutex::new(self.read.lock().expect("widget cache").clone()),
        }
    }
}

impl fmt::Debug for WidgetCache {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("WidgetCache")
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
    /// properties by name crc); check them with `Asset::find_u32`.
    pub crc_refs: Vec<usize>,
}

impl Asset {
    pub fn parse(bytes: Vec<u8>) -> Result<Asset> {
        ensure!(
            bytes.len() > Header::LEN,
            "PHX asset too short ({} bytes)",
            bytes.len()
        );
        ensure!(
            bytes[..Header::MAGIC.len()] == Header::MAGIC,
            "not a PHX asset (magic {:02x?})",
            &bytes[..5]
        );
        let header = Header(bytes[..Header::LEN].try_into().unwrap());

        let (types, types_end) = parse_types(&bytes)?;
        ensure!(!types.is_empty(), "class table is empty");
        let (heap, heap_end) = parse_heap(&bytes, types_end, types.len())?;

        // About one span per 7 bytes (font data aside, which is one): one
        // reserve with room, shrunk once decoded, instead of growing
        // through a dozen copies.
        let mut head = Vec::with_capacity(bytes.len() / 4 + 16);
        let sections = [
            Span::new(0..Header::LEN, Record::Header),
            Span::new(Header::LEN..types_end, Record::Types),
            Span::new(types_end..heap_end, Record::Heap),
        ];
        let section_count = sections.len();
        head.extend(sections);
        let schema = file_schema(&types).unwrap_or_default();
        // The body decodes into the span list after the header sections,
        // not into one of its own to be copied over.
        let mut wire = Wire::new(&bytes, &types, &schema, head);
        let parsed = (!schema.is_empty()).then(|| wire.parse(heap_end)).flatten();
        let (mut spans, properties, typed) = match parsed {
            Some(()) => (wire.spans, wire.properties, true),
            None => {
                let mut spans = wire.spans;
                spans.truncate(section_count);
                let mut body = Body::new(&bytes, &types);
                body.parse(heap_end, &heap)?;
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
            heap,
            spans,
            properties,
            typed,
            schema,
            self_names,
            field_spans,
            guid: None,
            enum_rules: std::sync::OnceLock::new(),
            widget_types: std::sync::OnceLock::new(),
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
    pub fn heap(&self) -> &Heap {
        &self.heap
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
    fn class_props(&self, class: u32) -> Option<&[&'static Prop]> {
        self.schema.get(&class).map(|v| v.as_slice())
    }

    pub fn root_class(&self) -> u32 {
        self.types[0].class
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

    fn strings(&self) -> impl Iterator<Item = (usize, &StrRecord)> {
        self.spans
            .iter()
            .enumerate()
            .filter_map(|(i, s)| match &s.record {
                Record::Str(r) => Some((i, r)),
                _ => None,
            })
    }

    fn references(&self) -> impl Iterator<Item = (usize, &Reference)> {
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

    /// Size of the array in the root's first pointer slot, when the heap
    /// starts `40 root, 42 n`. For widget templates this is the number of
    /// top-level widgets (`… <mask> 01 00 00 00 <n> 02 02 <id> …`).
    pub fn root_children(&self) -> Option<u32> {
        match self.heap.allocations.get(1) {
            Some(t) if t.kind == AllocKind::slots(WireType::OwnedPointer) => Some(t.count),
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
    fn find_u32(&self, value: u32) -> Vec<usize> {
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
            Record::Heap => encode_heap(&self.heap, out),
            Record::Object(o) => encode_object(o, out),
            Record::Str(s) => {
                if s.with_crc {
                    out.extend_from_slice(&s.crc().to_le_bytes());
                }
                write_str_payload(out, &s.text);
            }
            Record::Ref(r) => r.encode(out),
            Record::Color(rgb) => out.extend_from_slice(rgb),
            Record::Value(v) => v.encode(out),
            Record::Null => out.push(ObjectId::NULL),
            Record::Count(n) => varint::write(out, *n),
            Record::Default(v) => v.encode(out),
            Record::Bytes | Record::Opaque => out.extend_from_slice(&self.bytes[range.clone()]),
        }
    }

    /// Applies one edit and returns the re-parsed asset. The edit is rejected
    /// unless the result parses to the same span structure with every other
    /// span byte-identical.
    ///
    /// Length changes are safe in the container: there are no offset tables
    /// or size fields (header bytes 8..16 are constant, the heap byte count
    /// covers only the heap section, string lengths are per-record varints).
    pub fn apply(&self, edit: &Edit) -> Result<Edited> {
        self.apply_many(std::slice::from_ref(edit))
    }

    /// Applies several edits in one encode+parse round, under the same
    /// per-edit guarantees as [`Asset::apply`]: the span structure is
    /// preserved, edited spans re-parse to the new records, and every other
    /// span stays byte-identical. No two edits may target the same span.
    /// `old_crc`/`crc_refs` report the first text edit that changes a CRC'd
    /// string (the one consumers of a single `apply` would see).
    fn apply_many(&self, edits: &[Edit]) -> Result<Edited> {
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
    fn body_start(&self) -> usize {
        self.spans[2].range.end
    }

    /// Applies structural splices to the body (objects added, removed or
    /// moved; array counts changed) and re-derives what depends on it: the
    /// class table's heap instance counts and the heap section. `classes`
    /// adds class table entries (with their base chains) for objects of
    /// classes this file did not use before.
    ///
    /// The heap section is a function of the body, checked against all
    /// 4,682 installed assets: in file order, `0x40 1 <class index>` per heap
    /// object and `<element type code> n` per non-empty array (`0x42` pointer
    /// arrays, `0x08` u32 arrays), plus after a binding's target array one
    /// `0x51 n` run each for its source and target index arrays. The per-kind
    /// sums keep the file's order (descending kind in all but one shipped
    /// file); new kinds go last.
    // TODO(alex): new class table entries are appended; the serializer adds
    // them in first-use order. Not known whether the runtime cares.
    fn rebuild(&self, splices: &[Splice], classes: &[u32]) -> Result<Asset> {
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
        let mut allocations = Vec::new();
        for s in &wire.spans {
            match &s.record {
                Record::Object(o) if o.heap => allocations.push(Allocation {
                    kind: AllocKind::OBJECT,
                    count: 1,
                    class_index: Some(index[&o.class]),
                }),
                Record::Count(n) => {
                    let f = s.field.expect("counts belong to a field");
                    let class = class_of(f.object);
                    let prop = schema[&class][f.prop as usize - 1];
                    if *n > 0 {
                        allocations.push(Allocation {
                            kind: AllocKind(prop.type_byte()),
                            count: *n,
                            class_index: None,
                        });
                    }
                    if prop.name_crc == Prop::BINDING_TARGETS {
                        let sources = wire.spans.iter().find_map(|t| match (&t.record, t.field) {
                            (Record::Count(k), Some(g))
                                if g.object == f.object
                                    && schema[&class][g.prop as usize - 1].name_crc
                                        == Prop::BINDING_SOURCES =>
                            {
                                Some(*k)
                            }
                            _ => None,
                        });
                        for k in [sources.unwrap_or(0), *n] {
                            if k > 0 {
                                allocations.push(Allocation {
                                    kind: AllocKind::BINDING_INDICES,
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
        let mut sums: BTreeMap<AllocKind, u32> = BTreeMap::new();
        for t in allocations.iter().filter(|t| t.kind != AllocKind::OBJECT) {
            *sums.entry(t.kind).or_default() += t.count;
        }
        let mut counts: Vec<(AllocKind, u32)> = self
            .heap
            .counts
            .iter()
            .filter_map(|&(k, _)| Some((k, sums.remove(&k)?)))
            .collect();
        counts.extend(sums.into_iter().rev());
        let heap = Heap {
            counts,
            reserved: self.heap.reserved,
            allocations,
        };

        let mut out = Vec::with_capacity(body.len() + start);
        out.extend_from_slice(&self.header.0);
        encode_types(&types, &mut out);
        encode_heap(&heap, &mut out);
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

fn u32_at(b: &[u8], p: usize) -> Option<u32> {
    Some(u32::from_le_bytes(
        b.get(p..p.checked_add(CRC)?)?.try_into().unwrap(),
    ))
}

fn guid_at(b: &[u8], p: usize) -> Option<Guid> {
    Some(Guid(b.get(p..p.checked_add(ID)?)?.try_into().unwrap()))
}

fn write_str_payload(out: &mut Vec<u8>, s: &str) {
    varint::write(out, s.len() as u32);
    if !s.is_empty() {
        out.push(str_encoding(s));
        out.extend_from_slice(s.as_bytes());
    }
}

// The byte after a non-empty string's length (2 shipped strings are
// non-ASCII, both mojibake). Empty strings are the bare `00` length.
fn str_encoding(s: &str) -> u8 {
    if s.is_ascii() {
        StrRecord::ASCII
    } else {
        StrRecord::UTF8
    }
}

// `<varint len> <encoding> <bytes>` at `p`; the encoding byte must be the one
// write_str_payload would emit so the record re-encodes exactly.
fn read_str(b: &[u8], p: usize) -> Option<(String, usize)> {
    let (n, q) = varint::read(b, p)?;
    if n == 0 {
        return Some((String::new(), q));
    }
    let end = (q + ObjectId::TAG).checked_add(n as usize)?;
    let text = std::str::from_utf8(b.get(q + ObjectId::TAG..end)?).ok()?;
    (*b.get(q)? == str_encoding(text)).then(|| (text.to_owned(), end))
}

fn parse_types(b: &[u8]) -> Result<(Vec<TypeEntry>, usize)> {
    let (n, mut p) = varint::read(b, Header::LEN).context("class table count")?;
    ensure!(n <= MAX_TYPES, "class table claims {n} entries");
    let mut types = Vec::with_capacity(n as usize);
    for i in 0..n {
        let truncated = || format!("class table entry {i} truncated");
        let class = u32_at(b, p).with_context(truncated)?;
        let schema: [u8; ID] = b
            .get(p + CRC..p + CRC + ID)
            .with_context(truncated)?
            .try_into()
            .unwrap();
        let (instances, q) = varint::read(b, p + CRC + ID).with_context(truncated)?;
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
        check == b[Header::LEN..p],
        "class table uses non-canonical varints"
    );
    Ok((types, p))
}

fn encode_types(types: &[TypeEntry], out: &mut Vec<u8>) {
    varint::write(out, types.len() as u32);
    for t in types {
        out.extend_from_slice(&t.class.to_le_bytes());
        out.extend_from_slice(&t.schema);
        varint::write(out, t.instances);
    }
}

fn parse_heap(b: &[u8], start: usize, n_types: usize) -> Result<(Heap, usize)> {
    let bad = || format!("heap section at {start:#x} truncated");
    let (k, mut p) = varint::read(b, start).with_context(bad)?;
    ensure!(k <= 256, "heap section claims {k} count kinds");
    let mut counts = Vec::with_capacity(k as usize);
    for _ in 0..k {
        let kind = AllocKind(*b.get(p).with_context(bad)?);
        let (n, q) = varint::read(b, p + 1).with_context(bad)?;
        counts.push((kind, n));
        p = q;
    }
    let reserved = *b.get(p).with_context(bad)?;
    let (n_allocs, q) = varint::read(b, p + 1).with_context(bad)?;
    let (n_bytes, q) = varint::read(b, q).with_context(bad)?;
    ensure!(
        n_allocs <= MAX_ALLOCATIONS,
        "heap section claims {n_allocs} allocations"
    );
    let end = q
        .checked_add(n_bytes as usize)
        .filter(|&e| e <= b.len())
        .with_context(bad)?;
    let mut allocations = Vec::with_capacity(n_allocs as usize);
    let mut p = q;
    while p < end {
        let kind = AllocKind(b[p]);
        let (count, q) = varint::read(b, p + 1)
            .filter(|x| x.1 <= end)
            .with_context(bad)?;
        p = q;
        let class_index = if kind == AllocKind::OBJECT {
            let (ci, q) = varint::read(b, p)
                .filter(|x| x.1 <= end)
                .with_context(bad)?;
            ensure!(
                (ci as usize) < n_types,
                "allocation names class {ci} of {n_types}"
            );
            p = q;
            Some(ci)
        } else {
            None
        };
        allocations.push(Allocation {
            kind,
            count,
            class_index,
        });
    }
    ensure!(
        allocations.len() == n_allocs as usize,
        "heap section has {} allocations, its header says {n_allocs}",
        allocations.len()
    );
    let heap = Heap {
        counts,
        reserved,
        allocations,
    };
    let mut check = Vec::new();
    encode_heap(&heap, &mut check);
    ensure!(
        check == b[start..end],
        "heap section uses non-canonical varints"
    );
    Ok((heap, end))
}

fn encode_heap(heap: &Heap, out: &mut Vec<u8>) {
    varint::write(out, heap.counts.len() as u32);
    for &(kind, n) in &heap.counts {
        out.push(kind.0);
        varint::write(out, n);
    }
    out.push(heap.reserved);
    let mut body = Vec::new();
    for t in &heap.allocations {
        body.push(t.kind.0);
        varint::write(&mut body, t.count);
        if let Some(ci) = t.class_index {
            varint::write(&mut body, ci);
        }
    }
    varint::write(out, heap.allocations.len() as u32);
    varint::write(out, body.len() as u32);
    out.extend_from_slice(&body);
}

fn encode_object(o: &ObjectHeader, out: &mut Vec<u8>) {
    o.id.encode(out);
    out.extend_from_slice(&o.class.to_le_bytes());
    out.push(o.prop_count);
    out.extend_from_slice(&o.mask);
}

/// The header of a new object of `class`, written as the editor writes
/// every object it adds: the class's current version, each of its wire
/// properties present. Its values follow in wire order.
fn new_object(out: &mut Vec<u8>, id: ObjectId, class: u32) {
    let n = wire_props(class)
        .unwrap_or_else(|| panic!("class {class:08x} is not in the registry"))
        .len();
    // Presence bit `i` (MSB first) is wire prop `i`; bit 0 is never set.
    let mut mask = vec![0u8; (n + 1).div_ceil(8)];
    for i in 1..=n {
        let (byte, bit) = ObjectHeader::mask_bit(i);
        mask[byte] |= bit;
    }
    encode_object(
        &ObjectHeader {
            id,
            class,
            prop_count: u8::try_from(n + 1).expect("a class's props fit the count byte"),
            mask: mask.into(),
            heap: false,
        },
        out,
    );
}

impl Reference {
    /// Tag, class crc and id.
    const LEN: usize = ObjectId::TAG + CRC + ID;

    fn encode(&self, out: &mut Vec<u8>) {
        out.push(ObjectId::REFERENCE);
        out.extend_from_slice(&self.class.to_le_bytes());
        out.extend_from_slice(&self.id.0);
    }
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

// Schema-driven body decode: the root object, then every property value in
// wire order, recursing through pointers. All-or-nothing: None means some
// class or value shape is not covered and the caller falls back to Body.
struct Wire<'a> {
    b: &'a [u8],
    schema: &'a HashMap<u32, Vec<&'static Prop>>,
    // Classes the class table counts heap instances for, with the count.
    heap: HashMap<u32, u32>,
    // Rebuilding after a splice: heap-ness comes from Class::BY_VALUE and the
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
        if *b.get(start)? != ObjectId::OWNED {
            return None;
        }
        let id = ObjectId::Root(guid_at(b, start + ObjectId::TAG)?);
        let end = self.object_at(start, start + id.len(), id, None)?;
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
        let (prop_count, mask, e) = read_counts(self.b, c + CRC)?;
        let heap = if self.recount {
            !Class::BY_VALUE.contains(&class)
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
        if class == Class::COLOR && present.len() == Class::COLOR_BYTES {
            // One span for R, G, B so a color edit is one record.
            let rgb = self.b.get(p..p + Class::COLOR_BYTES)?.try_into().unwrap();
            self.push(
                p..p + Class::COLOR_BYTES,
                Record::Color(rgb),
                Some(Field {
                    object: obj,
                    prop: 1,
                }),
            );
            return Some(p + Class::COLOR_BYTES);
        }
        for i in present {
            let field = Field {
                object: obj,
                prop: i as u16,
            };
            p = self.value(p, field, props[i - 1])?;
        }
        if class == Class::CUSTOM_PROPERTY {
            self.custom_property(obj, props);
        }
        Some(p)
    }

    fn value(&mut self, p: usize, field: Field, prop: &Prop) -> Option<usize> {
        let ty = prop.wire_type()?;
        if !prop.is_array() {
            return self.element(p, field, ty, prop);
        }
        let (n, mut q) = varint::read(self.b, p)?;
        if n as usize > self.b.len() - q {
            return None;
        }
        self.push(p..q, Record::Count(n), Some(field));
        // Byte arrays are one span.
        if ty == WireType::U8 {
            let end = q + n as usize;
            if n > 0 {
                self.push(q..end, Record::Bytes, Some(field));
            }
            return Some(end);
        }
        for _ in 0..n {
            q = self.element(q, field, ty, prop)?;
        }
        Some(q)
    }

    fn element(&mut self, p: usize, field: Field, ty: WireType, prop: &Prop) -> Option<usize> {
        let b = self.b;
        let f32_at = |i: usize| u32_at(b, p + 4 * i).map(f32::from_bits);
        let value = match ty {
            WireType::Bool => match *b.get(p)? {
                v @ (0 | 1) => Value::Bool(v == 1),
                v => Value::U8(v),
            },
            WireType::U8 => Value::U8(*b.get(p)?),
            WireType::U16 => Value::U16(u16::from_le_bytes(b.get(p..p + 2)?.try_into().unwrap())),
            WireType::S32 => Value::S32(u32_at(b, p)? as i32),
            WireType::U32 => Value::U32(u32_at(b, p)?),
            WireType::U64 => Value::U64(u64::from_le_bytes(b.get(p..p + 8)?.try_into().unwrap())),
            WireType::Enum => Value::Enum {
                ty: prop.type_crc,
                value: u32_at(b, p)?,
            },
            WireType::F32 => Value::F32(f32_at(0)?),
            WireType::Vec2 => Value::Vec2([f32_at(0)?, f32_at(1)?]),
            WireType::Vec3 => Value::Vec3([f32_at(0)?, f32_at(1)?, f32_at(2)?]),
            WireType::Vec4 => Value::Vec4([f32_at(0)?, f32_at(1)?, f32_at(2)?, f32_at(3)?]),
            WireType::Id => Value::Guid(guid_at(b, p)?),
            WireType::Str => return self.string(p, field),
            WireType::Variant => return self.variant(p, field),
            WireType::Pointer | WireType::OwnedPointer => return self.pointer(p, field),
            WireType::AssetRef | WireType::AssetRef2 => return self.asset_ref(p, field),
            WireType::None => return None,
        };
        let width = ty.width()?;
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
        let ty = WireType::of(*b.get(p + 1)?)?;
        let value = match ty {
            WireType::None => DefaultValue::None,
            WireType::Bool => DefaultValue::Bool(*b.get(q)?),
            WireType::S32 => DefaultValue::S32(u32_at(b, q)? as i32),
            WireType::U32 => DefaultValue::U32(u32_at(b, q)?),
            WireType::F32 => DefaultValue::F32(u32_at(b, q)?),
            WireType::Vec2 | WireType::Vec3 | WireType::Vec4 => {
                let v: Option<Vec<u32>> = (0..ty.lanes()?).map(|i| u32_at(b, q + 4 * i)).collect();
                DefaultValue::Vec(v?)
            }
            WireType::Str => {
                let (text, end) = read_str(b, q)?;
                self.push(
                    p..end,
                    Record::Default(DefaultValue::Str(text)),
                    Some(field),
                );
                return Some(end);
            }
            WireType::Id => DefaultValue::ObjectId(guid_at(b, q)?),
            WireType::Pointer => {
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
        // No payload is `None`'s width.
        let end = q + ty.width().unwrap_or(0);
        self.push(p..end, Record::Default(value), Some(field));
        Some(end)
    }

    // `00` null, `01 <class> <id>` reference, `02 <id form> …` owned object.
    fn pointer(&mut self, p: usize, field: Field) -> Option<usize> {
        let b = self.b;
        match *b.get(p)? {
            ObjectId::OWNED => {
                let id = match *b.get(p + 1)? {
                    ObjectId::NO_ID => ObjectId::None,
                    ObjectId::WITH_ID => ObjectId::Id(guid_at(b, p + 2)?),
                    _ => return None,
                };
                self.object_at(p, p + id.len(), id, Some(field))
            }
            _ => self.asset_ref(p, field),
        }
    }

    fn asset_ref(&mut self, p: usize, field: Field) -> Option<usize> {
        let b = self.b;
        match *b.get(p)? {
            ObjectId::NULL => {
                self.push(p..p + 1, Record::Null, Some(field));
                Some(p + 1)
            }
            ObjectId::REFERENCE => {
                let r = Reference {
                    class: u32_at(b, p + 1)?,
                    id: guid_at(b, p + 5)?,
                };
                let end = p + Reference::LEN;
                self.push(p..end, Record::Ref(r), Some(field));
                Some(end)
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
                (Prop::UNIQUE_ID, Record::Value(Value::U32(v))) => uid = Some(*v),
                (Prop::PROPERTY_TYPE, Record::Value(Value::Enum { value, .. })) => {
                    kind = Some(*value)
                }
                (Prop::DEFAULT_VALUE, Record::Default(_)) if default.is_none() => {
                    default = Some(idx)
                }
                (Prop::OBJECT_TEMPLATE, Record::Ref(r)) => object_asset = Some(*r),
                (Prop::ENUM_ASSET, Record::Ref(r)) => enum_asset = Some(*r),
                (Prop::NAME, Record::Str(_)) => name = Some(idx),
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
// are anchored through the allocations, and between them the scan claims
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
        ref_classes.extend(Header::KINDS.iter().map(|k| k.2));
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

    fn parse(&mut self, start: usize, heap: &Heap) -> Result<()> {
        // Heap objects: one per 0x40 allocation, in the same pre-order as the body.
        let mut anchors: Vec<(usize, usize, ObjectHeader)> = Vec::new();
        let mut cursor = start;
        for (k, a) in heap
            .allocations
            .iter()
            .filter(|t| t.kind == AllocKind::OBJECT)
            .enumerate()
        {
            let class = self.types[a.class_index.unwrap() as usize].class;
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
        ensure!(!anchors.is_empty(), "heap section has no root object");

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
        if *b.get(p)? != ObjectId::OWNED {
            return None;
        }
        let id = ObjectId::Root(guid_at(b, p + ObjectId::TAG)?);
        if u32_at(b, p + id.len())? != class {
            return None;
        }
        let (prop_count, mask, end) = read_counts(b, p + id.len() + CRC)?;
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
        (from + ObjectId::None.len()..b.len().saturating_sub(CRC - 1))
            .filter(|&c| b[c..c + CRC] == needle)
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
        // The id form sits `len` bytes before the crc; an id is the 16
        // bytes right before it.
        let form = |id: ObjectId, f: u8| {
            let n = id.len();
            (c >= from + n && b[c - n] == ObjectId::OWNED && b[c - n + 1] == f).then_some(c - n)
        };
        let any_id = ObjectId::Id(Guid([0; 16]));
        let (start, id) = if let Some(start) = form(ObjectId::None, ObjectId::NO_ID) {
            (start, ObjectId::None)
        } else {
            let start = form(any_id, ObjectId::WITH_ID)?;
            (start, ObjectId::Id(guid_at(b, c - ID)?))
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
        let is_color = header.class == Class::COLOR
            && header.prop_count == Class::COLOR_PROPS
            && *header.mask == [Class::COLOR_MASK];
        self.push(s..e, Record::Object(header));
        let rgb = e..e + Class::COLOR_BYTES;
        if is_color && rgb.end <= limit {
            self.push(
                rgb.clone(),
                Record::Color(self.b[rgb.clone()].try_into().unwrap()),
            );
            return rgb.end;
        }
        e
    }

    fn crc_string(&self, p: usize, limit: usize) -> Option<(usize, StrRecord)> {
        let b = &self.b[..limit];
        let crc = u32_at(b, p)?;
        let (text, end) = read_str(b, p + CRC)?;
        let longest = p + CRC + MAX_SCAN_LEN_VARINT + ObjectId::TAG + text.len();
        if crc32(text.as_bytes()) != crc || end > longest {
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
        let (n, q) = varint::read(b, p)?;
        if !(MIN_SCAN_STR..=MAX_SCAN_STR as u32).contains(&n)
            || q > p + MAX_SCAN_LEN_VARINT
            || *b.get(q)? != StrRecord::ASCII
        {
            return None;
        }
        let end = q + ObjectId::TAG + n as usize;
        let text = std::str::from_utf8(b.get(q + ObjectId::TAG..end)?).ok()?;
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
        if b[p] == ObjectId::OWNED {
            for c in [ObjectId::None, ObjectId::Id(Guid([0; 16]))].map(|id| p + id.len()) {
                let Some(class) = u32_at(&b[..to], c) else {
                    continue;
                };
                if !self.classes.contains(&class) {
                    continue;
                }
                if let Some((s, e, header)) = self.header_ending_at(p, c, false) {
                    // Embedded objects in the shipped files never set mask bit
                    // 0 and always have some property present.
                    if s == p && e <= to && !header.bit(0) && header.present_count() > 0 {
                        return Some(self.object_values(s, e, header, to));
                    }
                }
            }
        }
        if b[p] == ObjectId::REFERENCE
            && p + Reference::LEN <= to
            && let Some(class) =
                u32_at(b, p + ObjectId::TAG).filter(|c| self.ref_classes.contains(c))
        {
            let r = Reference {
                class,
                id: guid_at(b, p + ObjectId::TAG + CRC)?,
            };
            self.push(p..p + Reference::LEN, Record::Ref(r));
            return Some(p + Reference::LEN);
        }
        if let Some((end, rec)) = self.bare_string(p, to) {
            self.push(p..end, Record::Str(rec));
            return Some(end);
        }
        None
    }
}

#[cfg(test)]
mod tests;
