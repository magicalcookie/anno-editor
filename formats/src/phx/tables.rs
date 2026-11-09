// Owns the per-game tables under formats/data/<game>/: the exes' class
// registries, Anno 117's game-object field ids and the other ids its UI files
// name. The files are embedded and parsed on first use; names borrow the
// embedded text.
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

use super::{Class, Legacy, Prop};
use crate::game::Game;

macro_rules! embed {
    ($path:literal) => {
        (
            $path,
            include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/data/", $path)),
        )
    };
}

/// One game's class registry, as its `classes.tsv` lists it.
#[derive(Debug)]
pub(super) struct Registry {
    /// Sorted by crc.
    pub(super) classes: Vec<Class>,
    /// Older class versions found in installed assets, file order.
    pub(super) legacy: Vec<Legacy>,
    /// Classes the exe no longer has: (crc, the base they decode as).
    pub(super) removed: Vec<(u32, u32)>,
}

impl Registry {
    pub(super) fn class(&self, crc: u32) -> Option<&Class> {
        let i = self.classes.binary_search_by_key(&crc, |c| c.crc).ok()?;
        Some(&self.classes[i])
    }
}

pub(super) fn registry(game: Game) -> &'static Registry {
    static ANNO117: ClassFile = ClassFile::new(embed!("anno117/classes.tsv"));
    static ANNO1800: ClassFile = ClassFile::new(embed!("anno1800/classes.tsv"));
    let file = match game {
        Game::Anno117 => &ANNO117,
        Game::Anno1800 => &ANNO1800,
    };
    file.registry.get_or_init(|| file.parse())
}

// `Class::props` and `Legacy::props` are `'static` slices, so the flat rows
// they point into get a static of their own, filled before the registry
// that borrows them.
struct ClassFile {
    file: (&'static str, &'static str),
    rows: OnceLock<(Vec<Prop>, Vec<u32>)>,
    registry: OnceLock<Registry>,
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
        // Each class and legacy row with the range of its rows in `props` /
        // `legacy_props`; the slices are taken once those have a home.
        let mut classes: Vec<(Class, Range<usize>)> = Vec::new();
        let mut legacy: Vec<(Legacy, Range<usize>)> = Vec::new();
        let mut removed = Vec::new();
        let mut props = Vec::new();
        let mut legacy_props = Vec::new();
        // The class or removed class the rows below belong to.
        let mut owner = None;
        for mut row in rows(self.file) {
            match row.col() {
                "class" => {
                    let class = Class {
                        crc: row.hex(),
                        name: row.name(),
                        base: row.hex(),
                        size: row.dec(),
                        hash: row.hash(),
                        props: &[],
                    };
                    owner = Some(class.crc);
                    classes.push((class, props.len()..props.len()));
                }
                "prop" => {
                    let Some((_, range)) = classes.last_mut().filter(|c| Some(c.0.crc) == owner)
                    else {
                        row.fail("prop row without a class row above it");
                    };
                    let name_crc = row.hex();
                    let name = row.name();
                    let ty = row.hex();
                    props.push(Prop {
                        name_crc,
                        name,
                        ty: u16::try_from(ty)
                            .unwrap_or_else(|_| row.fail("type word past 16 bits")),
                        type_crc: row.hex(),
                        mode: row.hex(),
                        offset: row.hex(),
                    });
                    range.end = props.len();
                }
                "legacy" => {
                    let Some(class) = owner else {
                        row.fail("legacy row without a class or removed row above it");
                    };
                    let hash = row.hash();
                    let start = legacy_props.len();
                    let list = row.col();
                    for crc in list.split(',').filter(|s| !s.is_empty()) {
                        legacy_props.push(row.parse_hex(crc));
                    }
                    let version = Legacy {
                        class,
                        hash,
                        props: &[],
                    };
                    legacy.push((version, start..legacy_props.len()));
                }
                "removed" => {
                    let crc = row.hex();
                    removed.push((crc, row.hex()));
                    owner = Some(crc);
                }
                kind => row.fail(format_args!("unknown row kind {kind:?}")),
            }
            row.end();
        }
        let (props, legacy_props) = self.rows.get_or_init(|| (props, legacy_props));
        Registry {
            classes: (classes.into_iter())
                .map(|(class, range)| Class {
                    props: &props[range],
                    ..class
                })
                .collect(),
            legacy: (legacy.into_iter())
                .map(|(version, range)| Legacy {
                    props: &legacy_props[range],
                    ..version
                })
                .collect(),
            removed,
        }
    }
}

/// Game-object fields in Anno117.exe: (id, name, PropertyType code, u32::MAX
/// for objects and unknown types), sorted by id.
pub(super) fn data_fields() -> &'static [(u32, &'static str, u32)] {
    static FIELDS: OnceLock<Vec<(u32, &'static str, u32)>> = OnceLock::new();
    FIELDS.get_or_init(|| {
        (rows(embed!("anno117/data_fields.tsv")))
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
            .collect()
    })
}

/// A game-object field by property id: (id, name, PropertyType code).
pub(super) fn data_field(id: u32) -> Option<(u32, &'static str, u32)> {
    let fields = data_fields();
    let i = fields.binary_search_by_key(&id, |f| f.0).ok()?;
    Some(fields[i])
}

/// Ids the shipped UI files use past `data_fields`, each list sorted by id.
pub(super) struct NamedIds {
    /// Game-object events, actions and fields: (id, name).
    pub(super) objects: Vec<(u32, &'static str)>,
    /// Component and effect custom properties: (UniqueId, component,
    /// property).
    pub(super) components: Vec<(u32, &'static str, &'static str)>,
}

pub(super) fn named_ids() -> &'static NamedIds {
    static NAMED: OnceLock<NamedIds> = OnceLock::new();
    NAMED.get_or_init(|| {
        let mut named = NamedIds {
            objects: Vec::new(),
            components: Vec::new(),
        };
        for mut row in rows(embed!("anno117/named_ids.tsv")) {
            match row.col() {
                "object" => named.objects.push((row.hex(), row.text())),
                "component" => named.components.push((row.hex(), row.text(), row.text())),
                kind => row.fail(format_args!("unknown row kind {kind:?}")),
            }
            row.end();
        }
        named
    })
}

pub(super) fn named_id(id: u32) -> Option<&'static str> {
    let objects = &named_ids().objects;
    let i = objects.binary_search_by_key(&id, |f| f.0).ok()?;
    Some(objects[i].1)
}

pub(super) fn component_prop(id: u32) -> Option<(&'static str, &'static str)> {
    let components = &named_ids().components;
    let i = components.binary_search_by_key(&id, |f| f.0).ok()?;
    Some((components[i].1, components[i].2))
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
        panic!("formats/data/{}:{}: {what}", self.file, self.line)
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

    fn end(mut self) {
        if let Some(extra) = self.cols.next() {
            self.fail(format_args!("extra column {extra:?}"));
        }
    }
}
