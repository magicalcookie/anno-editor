// Owns the exes' class registries (formats/data/<game>/classes.tsv) and the
// reader of the game-object field tables a game hands in (`DataFields`). The
// files are embedded and parsed on first use; names borrow the embedded text.
//
// Format: UTF-8, one row per line, tab-separated; `#` lines are comments and
// each file's header names its columns. Hex columns carry `0x`. An empty
// column is a missing name or an unknown value; a missing trailing column
// reads as empty, so an editor that strips trailing tabs does no harm. A bad
// row panics with its file and line: the data ships with the code, so it is
// a programmer mistake.

use std::fmt::Display;
use std::ops::Range;
use std::sync::OnceLock;

use super::{Class, Prop};

macro_rules! embed {
    ($path:literal) => {
        (
            concat!("formats/data/", $path),
            include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/data/", $path)),
        )
    };
}

/// One game's class registry, as its `classes.tsv` lists it. Every version
/// is a [`Class`] row with its own props: the exe's current ones and the
/// older ones found in installed assets alike, so a version is found by
/// (crc, schema hash) and decoded one way.
#[derive(Debug)]
pub(super) struct Registry {
    /// The exe's current versions, sorted by crc.
    pub(super) classes: Vec<Class>,
    /// Older versions found in installed assets, file order.
    pub(super) older: Vec<Class>,
    /// Classes the exe no longer has: (crc, the base they decode as).
    pub(super) removed: Vec<(u32, u32)>,
}

impl Registry {
    pub(super) fn class(&self, crc: u32) -> Option<&Class> {
        let i = self.classes.binary_search_by_key(&crc, |c| c.crc).ok()?;
        Some(&self.classes[i])
    }

    fn version(&self, crc: u32, hash: &[u8; 16]) -> Option<&Class> {
        (self.class(crc).into_iter())
            .chain(self.older.iter().filter(|c| c.crc == crc))
            .find(|c| c.hash == *hash)
    }
}

/// The exes' class registries, searched in this order. The UI format is
/// one across the games, so these are the codec's tables, not a game's: a
/// file's class table names each class's schema hash, and that version is
/// found in whichever registry has it, whichever game ships the file (117
/// kept 1800 screens and their classes). Only `class`, the current version
/// for what the editor writes and for a class with no hash to go by, takes
/// the first registry that has the class: 117's exe is the newer. A game
/// with classes of its own adds its `classes.tsv` here.
static REGISTRIES: [ClassFile; 2] = [
    ClassFile::new(embed!("anno117/classes.tsv")),
    ClassFile::new(embed!("anno1800/classes.tsv")),
];

pub(super) fn registries() -> impl Iterator<Item = &'static Registry> {
    REGISTRIES
        .iter()
        .map(|file| file.registry.get_or_init(|| file.parse()))
}

/// The current version of a class: the first registry's that has it.
pub(super) fn class(crc: u32) -> Option<&'static Class> {
    registries().find_map(|r| r.class(crc))
}

/// The class version a file names by its schema hash, current or older,
/// from any registry.
pub(super) fn class_version(crc: u32, hash: &[u8; 16]) -> Option<&'static Class> {
    registries().find_map(|r| r.version(crc, hash))
}

/// The base a class the exes dropped decodes as.
pub(super) fn removed_base(crc: u32) -> Option<u32> {
    registries()
        .flat_map(|r| &r.removed)
        .find(|r| r.0 == crc)
        .map(|r| r.1)
}

// `Class::props` are `'static` slices, so the flat rows they point into get
// a static of their own, filled before the registry that borrows them.
struct ClassFile {
    file: (&'static str, &'static str),
    /// Current versions' prop rows, then each older version's resolved list.
    rows: OnceLock<Vec<Prop>>,
    registry: OnceLock<Registry>,
}

/// An older version as its `legacy` row lists it, resolved once the
/// current versions are read.
struct Older {
    /// The class or removed class above it.
    owner: u32,
    hash: [u8; 16],
    /// Its own serialized props by name crc, in wire order.
    props: Vec<u32>,
    /// The `prop` rows after it: props the current version no longer has.
    dropped: Vec<Prop>,
}

impl ClassFile {
    const fn new(file: (&'static str, &'static str)) -> ClassFile {
        ClassFile {
            file,
            rows: OnceLock::new(),
            registry: OnceLock::new(),
        }
    }

    fn parse(&'static self) -> Registry {
        // Each current version with the range of its rows in `props`; the
        // slices are taken once `props` has a home.
        let mut classes: Vec<(Class, Range<usize>)> = Vec::new();
        let mut older: Vec<Older> = Vec::new();
        let mut removed: Vec<(u32, u32)> = Vec::new();
        let mut props = Vec::new();
        // The class or removed class the rows below belong to, and whether
        // a legacy row's props follow instead of the class's.
        let mut owner = None;
        let mut in_legacy = false;
        for mut row in rows(self.file) {
            match row.col() {
                "class" => {
                    let (crc, name, base) = (row.hex(), row.name(), row.hex());
                    // The runtime instance size: checked, not needed to decode.
                    row.dec();
                    let class = Class {
                        crc,
                        name,
                        base,
                        hash: row.hash(),
                        props: &[],
                    };
                    owner = Some(class.crc);
                    in_legacy = false;
                    classes.push((class, props.len()..props.len()));
                }
                "prop" => {
                    let prop = row.prop(in_legacy);
                    if in_legacy {
                        older.last_mut().unwrap().dropped.push(prop);
                        row.end();
                        continue;
                    }
                    let Some((_, range)) = classes.last_mut().filter(|c| Some(c.0.crc) == owner)
                    else {
                        row.fail("prop row without a class or legacy row above it");
                    };
                    props.push(prop);
                    range.end = props.len();
                }
                "legacy" => {
                    let Some(owner) = owner else {
                        row.fail("legacy row without a class or removed row above it");
                    };
                    let hash = row.hash();
                    let list = row.col();
                    let props = (list.split(',').filter(|s| !s.is_empty()))
                        .map(|crc| row.parse_hex(crc))
                        .collect();
                    older.push(Older {
                        owner,
                        hash,
                        props,
                        dropped: Vec::new(),
                    });
                    in_legacy = true;
                }
                "removed" => {
                    let crc = row.hex();
                    removed.push((crc, row.hex()));
                    owner = Some(crc);
                    in_legacy = false;
                }
                kind => row.fail(format_args!("unknown row kind {kind:?}")),
            }
            row.end();
        }
        // An older version's props: each by name crc from the current
        // version, else from the rows it lists itself, copied in wire order.
        let mut older_ranges = Vec::with_capacity(older.len());
        for o in &older {
            let current = classes.iter().find(|c| c.0.crc == o.owner);
            let start = props.len();
            for &crc in &o.props {
                let own =
                    current.and_then(|c| props[c.1.clone()].iter().find(|p| p.name_crc == crc));
                let Some(p) = own.or_else(|| o.dropped.iter().find(|p| p.name_crc == crc)) else {
                    panic!(
                        "{}: class {:08x}'s legacy version names prop {crc:08x}, which neither it nor the class has",
                        self.file.0, o.owner
                    );
                };
                props.push(p.clone());
            }
            let (name, base) = match current {
                Some((c, _)) => (c.name, c.base),
                None => {
                    let r = removed.iter().find(|r| r.0 == o.owner).unwrap();
                    (None, r.1)
                }
            };
            older_ranges.push((o.owner, name, base, o.hash, start..props.len()));
        }
        let props = self.rows.get_or_init(|| props);
        Registry {
            classes: (classes.into_iter())
                .map(|(class, range)| Class {
                    props: &props[range],
                    ..class
                })
                .collect(),
            older: (older_ranges.into_iter())
                .map(|(crc, name, base, hash, range)| Class {
                    crc,
                    name,
                    base,
                    hash,
                    props: &props[range],
                })
                .collect(),
            removed,
        }
    }
}

/// A game's game-object field ids, which the data paths of its screens'
/// bindings name (`TextData`, `Text`). Two tables in the layout above, which
/// the game embeds:
/// - `fields`: its exe's property rows, `id name kind`, sorted by id; kind is
///   the PropertyType code (0 bool, 1 int, 3 float, 4 string…), empty for
///   objects and unknown types.
/// - `named`: ids its UI files use past those, each kind sorted by id:
///   `object id name` (events, actions, fields of older exes) and
///   `component id component property` (component and effect custom
///   properties by UniqueId, for paths whose instance sits in a template
///   that does not ship).
pub struct DataFields {
    fields: &'static str,
    named: &'static str,
    /// Ids the exe types one way and the screens use another (ids collide
    /// across classes): [`DataFields::kind`] says unknown.
    unsure: &'static [u32],
    parsed: OnceLock<ParsedFields>,
}

struct ParsedFields {
    /// (id, name, PropertyType code or u32::MAX).
    fields: Vec<(u32, &'static str, u32)>,
    objects: Vec<(u32, &'static str)>,
    /// (UniqueId, component, property).
    components: Vec<(u32, &'static str, &'static str)>,
}

impl std::fmt::Debug for DataFields {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("DataFields")
    }
}

impl DataFields {
    pub const fn new(
        fields: &'static str,
        named: &'static str,
        unsure: &'static [u32],
    ) -> DataFields {
        DataFields {
            fields,
            named,
            unsure,
            parsed: OnceLock::new(),
        }
    }

    fn parsed(&self) -> &ParsedFields {
        self.parsed.get_or_init(|| {
            let fields: Vec<_> = (rows(("data_fields.tsv", self.fields)))
                .map(|mut row| {
                    let id = row.hex();
                    let name = row.text();
                    let kind = match row.col() {
                        "" => u32::MAX,
                        kind => row.parse_dec(kind),
                    };
                    row.end();
                    (id, name, kind)
                })
                .collect();
            let mut objects = Vec::new();
            let mut components = Vec::new();
            for mut row in rows(("named_ids.tsv", self.named)) {
                match row.col() {
                    "object" => objects.push((row.hex(), row.text())),
                    "component" => components.push((row.hex(), row.text(), row.text())),
                    kind => row.fail(format_args!("unknown row kind {kind:?}")),
                }
                row.end();
            }
            // Every lookup binary-searches these.
            assert!(
                fields.windows(2).all(|w| w[0].0 < w[1].0),
                "data_fields.tsv is not sorted by id"
            );
            assert!(
                objects.windows(2).all(|w| w[0].0 < w[1].0),
                "named_ids.tsv's objects are not sorted by id"
            );
            assert!(
                components.windows(2).all(|w| w[0].0 < w[1].0),
                "named_ids.tsv's components are not sorted by id"
            );
            ParsedFields {
                fields,
                objects,
                components,
            }
        })
    }

    /// The exe's property row for `id`: its name and PropertyType code,
    /// u32::MAX for objects and unknown types.
    pub fn field(&self, id: u32) -> Option<(&'static str, u32)> {
        let fields = &self.parsed().fields;
        let i = fields.binary_search_by_key(&id, |f| f.0).ok()?;
        Some((fields[i].1, fields[i].2))
    }

    /// A game-object field's, event's or action's name by its property id:
    /// the exe's property rows, then the other ids its files use (the scene
    /// transition's ShowSingle, the construction bar's ConstructionBarData
    /// in files kept from an older game).
    pub fn name(&self, id: u32) -> Option<&'static str> {
        self.field(id).map(|f| f.0).or_else(|| {
            let objects = &self.parsed().objects;
            let i = objects.binary_search_by_key(&id, |f| f.0).ok()?;
            Some(objects[i].1)
        })
    }

    /// A game-object field's PropertyType code by its property id; None for
    /// objects, unknown types and ids the screens use against their type.
    pub fn kind(&self, id: u32) -> Option<u32> {
        self.field(id)
            .map(|f| f.1)
            .filter(|&t| t != u32::MAX && !self.unsure.contains(&id))
    }

    /// A component's or effect's custom property by UniqueId: (component,
    /// property), `("Tween", "Started")`.
    pub fn component_property(&self, id: u32) -> Option<(&'static str, &'static str)> {
        let components = &self.parsed().components;
        let i = components.binary_search_by_key(&id, |f| f.0).ok()?;
        Some((components[i].1, components[i].2))
    }
}

/// The data rows of an embedded file, comments and blank lines skipped.
fn rows((file, text): (&'static str, &'static str)) -> impl Iterator<Item = Row> {
    (text.lines().enumerate())
        .filter(|(_, line)| !line.is_empty() && !line.starts_with('#'))
        .map(move |(i, line)| Row {
            file,
            line: i + 1,
            cols: line.split('\t'),
        })
}

/// One data row, read column by column.
struct Row {
    file: &'static str,
    line: usize,
    cols: std::str::Split<'static, char>,
}

impl Row {
    fn fail(&self, what: impl Display) -> ! {
        panic!("{}:{}: {what}", self.file, self.line)
    }

    /// The next column as written; "" past the last one.
    fn col(&mut self) -> &'static str {
        self.cols.next().unwrap_or("")
    }

    fn name(&mut self) -> Option<&'static str> {
        Some(self.col()).filter(|s| !s.is_empty())
    }

    /// A column that must not be empty.
    fn text(&mut self) -> &'static str {
        self.name().unwrap_or_else(|| self.fail("empty name"))
    }

    fn hex(&mut self) -> u32 {
        let s = self.col();
        self.parse_hex(s)
    }

    fn parse_hex(&self, s: &str) -> u32 {
        (s.strip_prefix("0x"))
            .and_then(|digits| u32::from_str_radix(digits, 16).ok())
            .unwrap_or_else(|| self.fail(format_args!("bad hex number {s:?}")))
    }

    fn dec(&mut self) -> u32 {
        let s = self.col();
        self.parse_dec(s)
    }

    fn parse_dec(&self, s: &str) -> u32 {
        (s.parse()).unwrap_or_else(|_| self.fail(format_args!("bad decimal number {s:?}")))
    }

    /// A schema hash: 32 hex digits, byte order as written.
    fn hash(&mut self) -> [u8; 16] {
        let s = self.col();
        if s.len() != 32 || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
            self.fail(format_args!("bad schema hash {s:?}"));
        }
        u128::from_str_radix(s, 16).unwrap().to_be_bytes()
    }

    /// A `prop` row's columns. An older version's own rows have no offset:
    /// no runtime instance has them.
    fn prop(&mut self, older: bool) -> Prop {
        let name_crc = self.hex();
        let name = self.name();
        let ty = self.hex();
        let prop = Prop {
            name_crc,
            name,
            ty: u16::try_from(ty).unwrap_or_else(|_| self.fail("type word past 16 bits")),
            type_crc: self.hex(),
            mode: self.hex(),
        };
        // The field's offset in the runtime instance: checked, not needed
        // to decode.
        match (older, self.col()) {
            (true, "") => {}
            (_, offset) => {
                self.parse_hex(offset);
            }
        }
        prop
    }

    fn end(mut self) {
        if let Some(extra) = self.cols.next() {
            self.fail(format_args!("extra column {extra:?}"));
        }
    }
}
