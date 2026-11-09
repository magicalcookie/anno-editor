// The two side files next to an island `.a7m`: the `.a7minfo` ADF the game
// reads to place the island on a map, and the `.a7me` XML only the Anno
// editor reads. Both round-trip every shipped file byte-exactly.

use anyhow::{Context, Result, bail, ensure};
use quick_xml::Reader;
use quick_xml::events::Event;

use crate::adf::{Adf, ItemId, utf16_bytes};

/// Island placement header (`.a7minfo`). Read by
/// `rd3d_CIslandMapInfo_deserialize` for every pool island at
/// map generation and for the island itself at session load.
#[derive(Debug, Clone, PartialEq)]
pub struct A7mInfo {
    /// World size in metres; equals gamedata `TerrainManager/WorldSize`. The
    /// game treats an island with a zero component as missing.
    pub map_size: [u32; 2],
    /// Only on some campaign islands.
    pub default_session_guid: Option<u32>,
    /// Buildable mask, one bit per 8 m cell (`map_size / 8`).
    pub active_map: ActiveMap,
    /// Bounding box of the set mask cells in metres: x0, y0, x1, y1
    /// (exclusive). Despite the name not x, y, w, h.
    pub active_rect: [u32; 4],
    /// Random-slot objects (mines, fertility slots) grouped by slot asset.
    pub slots: Slots,
    /// Radians; the map generator turns starter islands so this coast faces
    /// the open sea. The node is always written, the value only when set.
    pub start_coast_direction: Option<f32>,
    /// Editor sub-file versions (`FileTypeName`, `FileVersion`), in order.
    pub versions: Vec<(String, u32)>,
    /// Tile count per area id; the map generator sums the non-zero counts
    /// into a size class.
    pub area_tiles: Option<Vec<(u16, u32)>>,
    /// Area ids; seen instead of `AreaTiles` on some 3rd-party and large
    /// islands.
    pub area_indexes: Option<Vec<u16>>,
    /// Backslash path of the `.a7me`, e.g.
    /// `data\base\provinces\roman\islands\pool\x\x.a7me`.
    pub source_map: String,

    // Dictionary listing order of the parsed file. The engine binds names by
    // id, so the order only matters for byte-exact re-encodes; it is neither
    // id order nor any FNV bucket order we could reproduce. New files list
    // names in id order.
    dict_order: Option<(Vec<String>, Vec<String>)>,
    source: Option<Vec<u8>>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ActiveMap {
    pub width: u32,
    pub height: u32,
    /// Row-major, rows padded to 32 bits, LSB first.
    pub bits: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RandomSlotGroup {
    /// GUID of the random-slot asset these objects belong to.
    pub guid: u32,
    pub objects: Vec<RandomSlotObject>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Slots {
    Random(Vec<RandomSlotGroup>),
    /// 1800 groups by SlotType, and stores each object's asset GUID.
    Typed(Vec<TypedSlotGroup>),
}

#[derive(Debug, Clone, PartialEq)]
pub struct TypedSlotGroup {
    pub kind: u16,
    pub objects: Vec<TypedSlotObject>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TypedSlotObject {
    pub guid: u32,
    pub object: RandomSlotObject,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RandomSlotObject {
    /// gamedata object id of the placed slot object.
    pub object_id: u64,
    pub position: [f32; 3],
}

/// Metres per `ActiveMapGrid` cell on every shipped island.
pub const ACTIVE_CELL_METRES: u32 = 8;

/// The version list every shipped pool island carries.
const DEFAULT_VERSIONS: [(&str, u32); 9] = [
    ("EditorFile", 4),
    ("SessionSettings", 2),
    ("SessionSettings", 2),
    ("ObjectLabels", 2),
    ("TerrainSettings", 2),
    ("ObjectGroups", 3),
    ("ObjectGroups", 3),
    ("Regions", 2),
    ("Regions", 2),
];

// Smallest canonical ADF: just the root close record, two empty
// dictionaries and a -3 trailer. New files grow from it so ids are allocated
// in write order exactly like the engine writer does.
const EMPTY_ADF: [u8; 48] = {
    let mut b = [0u8; 48];
    // dictA count 0 at 8, dictB count 0 at 16, both padded to 8 bytes.
    // trailer at 24: w0=0, items=1, dictA=8, dictB=16, w5=8, -3
    b[28] = 1;
    b[32] = 8;
    b[36] = 16;
    b[40] = 8;
    b[44] = 0xfd;
    b[45] = 0xff;
    b[46] = 0xff;
    b[47] = 0xff;
    b
};

impl A7mInfo {
    /// Header for a new pool island: empty mask, the shipped version list and
    /// `source_map` pointing at the island's `.a7me` (backslashes, as
    /// shipped). Fill the mask with [`A7mInfo::set_active_map`].
    pub fn new(map_size: [u32; 2], a7me_path: &str) -> A7mInfo {
        let (w, h) = (
            map_size[0] / ACTIVE_CELL_METRES,
            map_size[1] / ACTIVE_CELL_METRES,
        );
        A7mInfo {
            map_size,
            default_session_guid: None,
            active_map: ActiveMap {
                width: w,
                height: h,
                bits: vec![0; w.div_ceil(32) as usize * 4 * h as usize],
            },
            active_rect: [0; 4],
            slots: Slots::Random(Vec::new()),
            start_coast_direction: None,
            versions: DEFAULT_VERSIONS
                .iter()
                .map(|&(n, v)| (n.to_string(), v))
                .collect(),
            area_tiles: None,
            area_indexes: None,
            source_map: a7me_path.replace('/', "\\"),
            dict_order: None,
            source: None,
        }
    }

    /// Replaces the buildable mask and recomputes `active_rect`.
    pub fn set_active_map(&mut self, width: u32, height: u32, cells: &[bool]) -> Result<()> {
        let (w, h) = (width as usize, height as usize);
        ensure!(
            cells.len() == w * h,
            "active map {width}x{height}: {} cells",
            cells.len()
        );
        let stride = w.div_ceil(32) * 4;
        let mut bits = vec![0u8; stride * h];
        let mut rect: Option<[usize; 4]> = None;
        for (i, _) in cells.iter().enumerate().filter(|c| *c.1) {
            let (x, y) = (i % w, i / w);
            bits[y * stride + x / 8] |= 1 << (x % 8);
            let r = rect.get_or_insert([x, y, x, y]);
            *r = [r[0].min(x), r[1].min(y), r[2].max(x), r[3].max(y)];
        }
        let cell_w = (self.map_size[0] / width.max(1)) as usize;
        let cell_h = (self.map_size[1] / height.max(1)) as usize;
        // TODO(alex): unknown what the editor writes for an empty mask; zeros.
        self.active_rect = rect.map_or([0; 4], |r| {
            [
                (r[0] * cell_w) as u32,
                (r[1] * cell_h) as u32,
                ((r[2] + 1) * cell_w) as u32,
                ((r[3] + 1) * cell_h) as u32,
            ]
        });
        self.active_map = ActiveMap {
            width,
            height,
            bits,
        };
        Ok(())
    }

    pub fn parse(bytes: &[u8]) -> Result<A7mInfo> {
        let adf = Adf::decode(bytes).context(".a7minfo")?;
        let root = adf.root();
        let mut info = A7mInfo::new([0, 0], "");
        info.versions.clear();
        for item in adf.children(root) {
            let name = adf.name(item).unwrap_or("");
            match (name, adf.is_node(item)) {
                ("MapSize", false) => {
                    let v = adf.read_u64(item)?;
                    info.map_size = [v as u32, (v >> 32) as u32];
                }
                ("DefaultSessionGUID", false) => {
                    info.default_session_guid = Some(adf.read_u32(item)?)
                }
                ("ActiveMapGrid", true) => {
                    let x = adf.read_u32(field(&adf, item, "x")?)?;
                    let y = adf.read_u32(field(&adf, item, "y")?)?;
                    let bits = adf.payload(field(&adf, item, "bits")?).to_vec();
                    only(&adf, item, &["x", "y", "bits"])?;
                    ensure!(
                        bits.len() == x.div_ceil(32) as usize * 4 * y as usize,
                        "ActiveMapGrid {x}x{y}: {} bytes",
                        bits.len()
                    );
                    info.active_map = ActiveMap {
                        width: x,
                        height: y,
                        bits,
                    };
                }
                ("ActiveMapRect", false) => {
                    info.active_rect = u32s(adf.payload(item))?;
                }
                ("ObjectMetaInfo", true) => info.parse_meta(&adf, item)?,
                ("Version", true) => {
                    only(&adf, item, &["VersionList"])?;
                    let list = adf
                        .child_node(item, "VersionList")
                        .context("Version: no VersionList")?;
                    for entry in adf.children(list) {
                        only(&adf, entry, &["FileTypeName", "FileVersion"])?;
                        let name = adf.payload(field(&adf, entry, "FileTypeName")?);
                        let ver = adf.read_u32(field(&adf, entry, "FileVersion")?)?;
                        info.versions.push((String::from_utf8(name.to_vec())?, ver));
                    }
                }
                ("AreaTiles", true) => {
                    let kids: Vec<ItemId> = adf.children(item).collect();
                    ensure!(kids.len().is_multiple_of(2), "AreaTiles: odd entry count");
                    let mut tiles = Vec::new();
                    for pair in kids.chunks(2) {
                        tiles.push((adf.read_u16(pair[0])?, adf.read_u32(pair[1])?));
                    }
                    info.area_tiles = Some(tiles);
                }
                ("AreaIndexes", true) => {
                    let ids = adf.children(item).map(|c| adf.read_u16(c));
                    info.area_indexes = Some(ids.collect::<Result<_>>()?);
                }
                ("MapSource", true) => {
                    if !matches!(info.slots, Slots::Typed(_)) {
                        only(&adf, item, &["SourceMap"])?;
                    }
                    info.source_map = adf.read_utf16(field(&adf, item, "SourceMap")?)?;
                }
                _ => bail!(".a7minfo: unknown top-level field {name:?}"),
            }
        }
        if matches!(info.slots, Slots::Typed(_)) {
            info.source = Some(bytes.to_vec());
        } else {
            info.dict_order = Some(dict_names(bytes)?);
        }
        Ok(info)
    }

    /// Slots by asset GUID, including 1800's typed slot records.
    pub fn random_slots(&self) -> Vec<RandomSlotGroup> {
        match &self.slots {
            Slots::Random(groups) => groups.clone(),
            Slots::Typed(groups) => {
                let mut by_guid = std::collections::BTreeMap::<u32, Vec<RandomSlotObject>>::new();
                for o in groups.iter().flat_map(|g| &g.objects) {
                    by_guid.entry(o.guid).or_default().push(o.object);
                }
                by_guid
                    .into_iter()
                    .map(|(guid, objects)| RandomSlotGroup { guid, objects })
                    .collect()
            }
        }
    }

    fn parse_meta(&mut self, adf: &Adf, meta: ItemId) -> Result<()> {
        self.start_coast_direction = adf
            .value(meta, "StartCoastDirection/value")
            .map(|v| adf.read_f32(v))
            .transpose()?;
        if let Some(slots) = adf.child_node(meta, "SlotObjects") {
            only(adf, meta, &["SlotObjects", "StartCoastDirection"])?;
            let kids: Vec<_> = adf.children(slots).collect();
            ensure!(kids.len().is_multiple_of(2), "SlotObjects: odd entry count");
            let mut groups = Vec::new();
            for pair in kids.as_chunks::<2>().0 {
                let kind = adf
                    .value(pair[0], "value/id")
                    .map(|v| adf.read_u16(v))
                    .transpose()?
                    .unwrap_or(0);
                let mut objects = Vec::new();
                for obj in adf.children(pair[1]) {
                    only(adf, obj, &["ObjectId", "ObjectGuid", "Position"])?;
                    objects.push(TypedSlotObject {
                        guid: adf.read_u32(field(adf, obj, "ObjectGuid")?)?,
                        object: RandomSlotObject {
                            object_id: adf.read_u64(field(adf, obj, "ObjectId")?)?,
                            position: adf.read_f32s(field(adf, obj, "Position")?)?,
                        },
                    });
                }
                groups.push(TypedSlotGroup { kind, objects });
            }
            self.slots = Slots::Typed(groups);
            return Ok(());
        }
        only(adf, meta, &["RandomSlotObjects", "StartCoastDirection"])?;
        let slots = adf
            .child_node(meta, "RandomSlotObjects")
            .context("ObjectMetaInfo: no RandomSlotObjects")?;
        // Alternating (u32 guid value, unnamed node of unnamed object nodes).
        let kids: Vec<ItemId> = adf.children(slots).collect();
        ensure!(
            kids.len().is_multiple_of(2),
            "RandomSlotObjects: odd entry count"
        );
        for pair in kids.chunks(2) {
            ensure!(
                !adf.is_node(pair[0]) && adf.is_node(pair[1]),
                "RandomSlotObjects: expected guid then object list"
            );
            let mut group = RandomSlotGroup {
                guid: adf.read_u32(pair[0])?,
                objects: Vec::new(),
            };
            for obj in adf.children(pair[1]) {
                only(adf, obj, &["ObjectId", "Position"])?;
                group.objects.push(RandomSlotObject {
                    object_id: adf.read_u64(field(adf, obj, "ObjectId")?)?,
                    position: adf.read_f32s(field(adf, obj, "Position")?)?,
                });
            }
            let Slots::Random(groups) = &mut self.slots else {
                unreachable!()
            };
            groups.push(group);
        }
        let coast = adf
            .child_node(meta, "StartCoastDirection")
            .context("ObjectMetaInfo: no StartCoastDirection")?;
        only(adf, coast, &["value"])?;
        self.start_coast_direction = adf
            .child_value(coast, "value")
            .map(|v| adf.read_f32(v))
            .transpose()?;
        Ok(())
    }

    /// Writes fields in the order of every shipped file. Ids come out in
    /// first-use order like the engine writer allocates them. `AreaIndexes`
    /// and `AreaTiles` never ship together; their relative order is a guess.
    pub fn encode(&self) -> Vec<u8> {
        self.build().expect("fresh ADF only takes valid names")
    }

    fn build(&self) -> Result<Vec<u8>> {
        if let Some(source) = &self.source {
            return self.patch_source(source);
        }
        let mut adf = Adf::decode(&EMPTY_ADF)?;
        let root = adf.root();
        let [w, h] = self.map_size;
        adf.add_value(
            root,
            Some("MapSize"),
            &(u64::from(w) | u64::from(h) << 32).to_le_bytes(),
        )?;
        if let Some(guid) = self.default_session_guid {
            adf.add_value(root, Some("DefaultSessionGUID"), &guid.to_le_bytes())?;
        }
        let grid = adf.add_node(root, Some("ActiveMapGrid"))?;
        adf.add_value(grid, Some("x"), &self.active_map.width.to_le_bytes())?;
        adf.add_value(grid, Some("y"), &self.active_map.height.to_le_bytes())?;
        adf.add_value(grid, Some("bits"), &self.active_map.bits)?;
        let rect: Vec<u8> = self
            .active_rect
            .iter()
            .flat_map(|v| v.to_le_bytes())
            .collect();
        adf.add_value(root, Some("ActiveMapRect"), &rect)?;

        let meta = adf.add_node(root, Some("ObjectMetaInfo"))?;
        let slots = adf.add_node(meta, Some("RandomSlotObjects"))?;
        let Slots::Random(groups) = &self.slots else {
            bail!("typed slot headers need a shipped 1800 template");
        };
        for group in groups {
            adf.add_value(slots, None, &group.guid.to_le_bytes())?;
            let list = adf.add_node(slots, None)?;
            for obj in &group.objects {
                let o = adf.add_node(list, None)?;
                adf.add_value(o, Some("ObjectId"), &obj.object_id.to_le_bytes())?;
                let pos: Vec<u8> = obj.position.iter().flat_map(|v| v.to_le_bytes()).collect();
                adf.add_value(o, Some("Position"), &pos)?;
            }
        }
        let coast = adf.add_node(meta, Some("StartCoastDirection"))?;
        if let Some(dir) = self.start_coast_direction {
            adf.add_value(coast, Some("value"), &dir.to_le_bytes())?;
        }

        let version = adf.add_node(root, Some("Version"))?;
        let list = adf.add_node(version, Some("VersionList"))?;
        for (name, ver) in &self.versions {
            let e = adf.add_node(list, None)?;
            adf.add_value(e, Some("FileTypeName"), name.as_bytes())?;
            adf.add_value(e, Some("FileVersion"), &ver.to_le_bytes())?;
        }
        if let Some(indexes) = &self.area_indexes {
            let node = adf.add_node(root, Some("AreaIndexes"))?;
            for id in indexes {
                adf.add_value(node, None, &id.to_le_bytes())?;
            }
        }
        if let Some(tiles) = &self.area_tiles {
            let node = adf.add_node(root, Some("AreaTiles"))?;
            for (id, count) in tiles {
                adf.add_value(node, None, &id.to_le_bytes())?;
                adf.add_value(node, None, &count.to_le_bytes())?;
            }
        }
        let source = adf.add_node(root, Some("MapSource"))?;
        adf.add_value(source, Some("SourceMap"), &utf16_bytes(&self.source_map))?;

        let mut bytes = adf.encode();
        if let Some((a, b)) = &self.dict_order {
            reorder_dicts(&mut bytes, a, b);
        }
        Ok(bytes)
    }

    fn patch_source(&self, source: &[u8]) -> Result<Vec<u8>> {
        let old = Self::parse(source)?;
        let mut adf = Adf::decode(source)?;
        let root = adf.root();
        let set = |adf: &mut Adf, path: &str, bytes: &[u8]| -> Result<()> {
            let item = adf
                .value(root, path)
                .with_context(|| format!("header has no {path}"))?;
            adf.set_payload(item, bytes);
            Ok(())
        };
        let size: Vec<_> = self.map_size.iter().flat_map(|v| v.to_le_bytes()).collect();
        set(&mut adf, "MapSize", &size)?;
        set(
            &mut adf,
            "ActiveMapGrid/x",
            &self.active_map.width.to_le_bytes(),
        )?;
        set(
            &mut adf,
            "ActiveMapGrid/y",
            &self.active_map.height.to_le_bytes(),
        )?;
        set(&mut adf, "ActiveMapGrid/bits", &self.active_map.bits)?;
        let rect: Vec<_> = self
            .active_rect
            .iter()
            .flat_map(|v| v.to_le_bytes())
            .collect();
        set(&mut adf, "ActiveMapRect", &rect)?;
        set(
            &mut adf,
            "MapSource/SourceMap",
            &utf16_bytes(&self.source_map),
        )?;
        if old.default_session_guid != self.default_session_guid {
            set_optional(
                &mut adf,
                root,
                "DefaultSessionGUID",
                self.default_session_guid
                    .map(|v| v.to_le_bytes().to_vec())
                    .as_deref(),
            )?;
        }
        if old.start_coast_direction != self.start_coast_direction {
            let meta = adf
                .child_node(root, "ObjectMetaInfo")
                .context("no ObjectMetaInfo")?;
            let coast = match adf.child_node(meta, "StartCoastDirection") {
                Some(n) => n,
                None => adf.add_node(meta, Some("StartCoastDirection"))?,
            };
            set_optional(
                &mut adf,
                coast,
                "value",
                self.start_coast_direction
                    .map(|v| v.to_le_bytes().to_vec())
                    .as_deref(),
            )?;
        }
        if old.versions != self.versions {
            let list = adf
                .node(root, "Version/VersionList")
                .context("no VersionList")?;
            adf.clear_children(list);
            for (name, version) in &self.versions {
                let entry = adf.add_node(list, None)?;
                adf.add_value(entry, Some("FileTypeName"), name.as_bytes())?;
                adf.add_value(entry, Some("FileVersion"), &version.to_le_bytes())?;
            }
        }
        if old.area_indexes != self.area_indexes || old.area_tiles != self.area_tiles {
            for name in ["AreaIndexes", "AreaTiles"] {
                if let Some(n) = adf.child_node(root, name) {
                    adf.remove(root, n);
                }
            }
            if let Some(indexes) = &self.area_indexes {
                let list = adf.add_node(root, Some("AreaIndexes"))?;
                for id in indexes {
                    adf.add_value(list, None, &id.to_le_bytes())?;
                }
            }
            if let Some(tiles) = &self.area_tiles {
                let list = adf.add_node(root, Some("AreaTiles"))?;
                for (id, count) in tiles {
                    adf.add_value(list, None, &id.to_le_bytes())?;
                    adf.add_value(list, None, &count.to_le_bytes())?;
                }
            }
        }
        if old.slots != self.slots {
            let Slots::Typed(groups) = &self.slots else {
                bail!("cannot change header slot format")
            };
            let slots = adf
                .node(root, "ObjectMetaInfo/SlotObjects")
                .context("no SlotObjects")?;
            adf.clear_children(slots);
            for group in groups {
                let key = adf.add_node(slots, None)?;
                let value = adf.add_node(key, Some("value"))?;
                if group.kind != 0 {
                    adf.add_value(value, Some("id"), &group.kind.to_le_bytes())?;
                }
                let list = adf.add_node(slots, None)?;
                for obj in &group.objects {
                    let node = adf.add_node(list, None)?;
                    adf.add_value(node, Some("ObjectId"), &obj.object.object_id.to_le_bytes())?;
                    adf.add_value(node, Some("ObjectGuid"), &obj.guid.to_le_bytes())?;
                    let pos: Vec<_> = obj
                        .object
                        .position
                        .iter()
                        .flat_map(|v| v.to_le_bytes())
                        .collect();
                    adf.add_value(node, Some("Position"), &pos)?;
                }
            }
        }
        Ok(adf.encode())
    }
}

fn set_optional(adf: &mut Adf, node: ItemId, name: &str, bytes: Option<&[u8]>) -> Result<()> {
    match (adf.child_value(node, name), bytes) {
        (Some(item), Some(bytes)) => adf.set_payload(item, bytes),
        (Some(item), None) => {
            adf.remove(node, item);
        }
        (None, Some(bytes)) => {
            adf.add_value(node, Some(name), bytes)?;
        }
        (None, None) => {}
    }
    Ok(())
}

fn field(adf: &Adf, node: ItemId, name: &str) -> Result<ItemId> {
    adf.children(node)
        .find(|&c| adf.name(c) == Some(name))
        .with_context(|| format!("missing field {name}"))
}

// Fails on children the typed model would drop.
fn only(adf: &Adf, node: ItemId, names: &[&str]) -> Result<()> {
    for c in adf.children(node) {
        let n = adf.name(c).unwrap_or("");
        ensure!(
            names.contains(&n),
            "unexpected field {n:?} under {:?}",
            adf.name(node)
        );
    }
    Ok(())
}

fn u32s<const N: usize>(bytes: &[u8]) -> Result<[u32; N]> {
    ensure!(
        bytes.len() == N * 4,
        "expected {} bytes, got {}",
        N * 4,
        bytes.len()
    );
    Ok(std::array::from_fn(|i| {
        u32::from_le_bytes(bytes[i * 4..i * 4 + 4].try_into().unwrap())
    }))
}

// Names in listing order of the two dictionaries of a -3 file.
fn dict_names(bytes: &[u8]) -> Result<(Vec<String>, Vec<String>)> {
    let n = bytes.len();
    ensure!(n >= 24, "ADF too small");
    let word = |o: usize| u32::from_le_bytes(bytes[o..o + 4].try_into().unwrap()) as usize;
    ensure!(
        word(n - 4) as u32 == 0xffff_fffd,
        ".a7minfo: expected a -3 trailer"
    );
    let read = |off: usize| -> Result<Vec<String>> {
        let count = word(off);
        let mut p = off + 4 + count * 2;
        let mut names = Vec::with_capacity(count);
        for _ in 0..count {
            let len = bytes[p..]
                .iter()
                .position(|&b| b == 0)
                .context("dictionary name")?;
            names.push(String::from_utf8(bytes[p..p + len].to_vec())?);
            p += len + 1;
        }
        Ok(names)
    };
    Ok((read(word(n - 16))?, read(word(n - 12))?))
}

// Permutes the dictionary listings of a freshly encoded file into `order`.
// Same entries, same byte length, so offsets and padding stay valid. Names
// missing from `order` (fields added since parsing) keep id order at the end.
fn reorder_dicts(bytes: &mut [u8], order_a: &[String], order_b: &[String]) {
    let n = bytes.len();
    let word = |b: &[u8], o: usize| u32::from_le_bytes(b[o..o + 4].try_into().unwrap()) as usize;
    for (at, order) in [(n - 16, order_a), (n - 12, order_b)] {
        let off = word(bytes, at);
        let count = word(bytes, off);
        let mut entries: Vec<(u16, Vec<u8>)> = Vec::with_capacity(count);
        let mut p = off + 4 + count * 2;
        for i in 0..count {
            let id_at = off + 4 + i * 2;
            let id = u16::from_le_bytes([bytes[id_at], bytes[id_at + 1]]);
            let len = bytes[p..].iter().position(|&b| b == 0).unwrap();
            entries.push((id, bytes[p..p + len].to_vec()));
            p += len + 1;
        }
        let rank = |name: &[u8]| {
            order
                .iter()
                .position(|o| o.as_bytes() == name)
                .unwrap_or(usize::MAX)
        };
        entries.sort_by_key(|e| rank(&e.1));
        let mut q = off + 4;
        for (id, _) in &entries {
            bytes[q..q + 2].copy_from_slice(&id.to_le_bytes());
            q += 2;
        }
        for (_, name) in &entries {
            bytes[q..q + name.len()].copy_from_slice(name);
            bytes[q + name.len()] = 0;
            q += name.len() + 1;
        }
    }
}

/// Anno editor project file (`.a7me`). The game never opens it (no `.a7me`
/// string in the executable); only `.a7minfo` `SourceMap` points at it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct A7me {
    /// `FileVersion`; 4 on every shipped file.
    pub file_version: u32,
    /// Missing on 13 of 89 shipped files, `0` on the rest.
    pub scenario_guid: Option<u64>,
    /// Island size in metres (`X`, `Z`), equals `A7mInfo::map_size`.
    pub dimensions: [u32; 2],
    /// Editor chunk size in metres; 64 everywhere.
    pub chunk_size: [u32; 2],
    /// Chunk names per column, `"<x>x<z>"`. Every shipped file lists the full
    /// grid, see [`A7me::new`].
    pub chunks: Vec<Vec<String>>,
}

impl A7me {
    /// Editor meta for a new island: full chunk grid of 64 m chunks.
    pub fn new(dimensions: [u32; 2]) -> A7me {
        let chunk = 64;
        let (cx, cz) = (dimensions[0].div_ceil(chunk), dimensions[1].div_ceil(chunk));
        A7me {
            file_version: 4,
            scenario_guid: Some(0),
            dimensions,
            chunk_size: [chunk, chunk],
            chunks: (0..cx)
                .map(|x| (0..cz).map(|z| format!("{x}x{z}")).collect())
                .collect(),
        }
    }

    pub fn parse(bytes: &[u8]) -> Result<A7me> {
        let mut reader = Reader::from_reader(bytes);
        let mut me = A7me {
            file_version: 0,
            scenario_guid: None,
            dimensions: [0; 2],
            chunk_size: [0; 2],
            chunks: Vec::new(),
        };
        let mut path: Vec<String> = Vec::new();
        let mut buf = Vec::new();
        loop {
            match reader.read_event_into(&mut buf)? {
                Event::Start(e) => {
                    let name = String::from_utf8(e.name().as_ref().to_vec())?;
                    if path.len() == 2 && path[1] == "Chunks" && name == "Column" {
                        me.chunks.push(Vec::new());
                    }
                    path.push(name);
                }
                Event::End(_) => {
                    path.pop();
                }
                Event::Text(t) => {
                    let text = t.decode()?;
                    let text = text.trim();
                    if text.is_empty() {
                        continue;
                    }
                    let p: Vec<&str> = path.iter().skip(1).map(String::as_str).collect();
                    let num = || {
                        text.parse::<u64>()
                            .with_context(|| format!("{p:?}: {text:?}"))
                    };
                    match p.as_slice() {
                        ["FileVersion"] => me.file_version = num()? as u32,
                        ["ScenarioGuid"] => me.scenario_guid = Some(num()?),
                        ["Dimensions", "X"] => me.dimensions[0] = num()? as u32,
                        ["Dimensions", "Z"] => me.dimensions[1] = num()? as u32,
                        ["ChunkSize", "X"] => me.chunk_size[0] = num()? as u32,
                        ["ChunkSize", "Z"] => me.chunk_size[1] = num()? as u32,
                        ["Chunks", "Column", "Chunk"] => me
                            .chunks
                            .last_mut()
                            .context("Chunk outside Column")?
                            .push(text.to_string()),
                        _ => bail!(".a7me: unknown element {p:?}"),
                    }
                }
                Event::Eof => break,
                _ => {}
            }
            buf.clear();
        }
        ensure!(me.file_version != 0, ".a7me: no FileVersion");
        Ok(me)
    }

    /// The editor's layout: two-space indent, CRLF, no declaration, no
    /// trailing newline.
    pub fn encode(&self) -> Vec<u8> {
        let mut s = String::from("<AnnoEditorLevel>\r\n");
        s += &format!("  <FileVersion>{}</FileVersion>\r\n", self.file_version);
        if let Some(guid) = self.scenario_guid {
            s += &format!("  <ScenarioGuid>{guid}</ScenarioGuid>\r\n");
        }
        for (tag, [x, z]) in [
            ("Dimensions", self.dimensions),
            ("ChunkSize", self.chunk_size),
        ] {
            s += &format!("  <{tag}>\r\n    <X>{x}</X>\r\n    <Z>{z}</Z>\r\n  </{tag}>\r\n");
        }
        s += "  <Chunks>\r\n";
        for column in &self.chunks {
            s += "    <Column>\r\n";
            for chunk in column {
                s += &format!("      <Chunk>{chunk}</Chunk>\r\n");
            }
            s += "    </Column>\r\n";
        }
        s += "  </Chunks>\r\n</AnnoEditorLevel>";
        s.into_bytes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vfs::{FileId, Install};

    fn install() -> Option<Install> {
        let Some(dir) = std::env::var_os("ANNO117_DIR") else {
            eprintln!("skip: ANNO117_DIR unset");
            return None;
        };
        Some(Install::open(dir).unwrap())
    }

    fn shipped(install: &Install, ext: &str) -> Vec<String> {
        (0..install.file_count() as u32)
            .map(|i| install.file_path(FileId(i)).to_string())
            .filter(|p| p.ends_with(ext))
            .collect()
    }

    #[test]
    fn every_shipped_a7minfo_round_trips() {
        let Some(install) = install() else {
            return;
        };
        let paths = shipped(&install, ".a7minfo");
        assert!(paths.len() >= 89, "{} files", paths.len());
        for path in &paths {
            let bytes = install.read(path).unwrap();
            let info = A7mInfo::parse(&bytes).unwrap_or_else(|e| panic!("{path}: {e:#}"));
            assert!(info.encode() == bytes, "{path}: re-encode differs");
            // Semantics every shipped island agrees on.
            let grid = &info.active_map;
            assert_eq!(
                [grid.width, grid.height].map(|c| c * ACTIVE_CELL_METRES),
                info.map_size,
                "{path}"
            );
            let me = path.replace(".a7minfo", ".a7me").replace('/', "\\");
            assert!(
                info.source_map.eq_ignore_ascii_case(&me),
                "{path}: {}",
                info.source_map
            );
            let mut fresh = info.clone();
            let cells = unpack(grid);
            fresh
                .set_active_map(grid.width, grid.height, &cells)
                .unwrap();
            assert_eq!(fresh.active_rect, info.active_rect, "{path}: rect");
            assert_eq!(fresh.active_map.bits, grid.bits, "{path}: bits");
        }
    }

    #[test]
    fn every_shipped_a7me_round_trips() {
        let Some(install) = install() else {
            return;
        };
        for path in shipped(&install, ".a7me") {
            let bytes = install.read(&path).unwrap();
            let me = A7me::parse(&bytes).unwrap_or_else(|e| panic!("{path}: {e:#}"));
            assert!(me.encode() == bytes, "{path}: re-encode differs");
            // A full chunk grid, so `A7me::new` reproduces the shipped file.
            let mut fresh = A7me::new(me.dimensions);
            fresh.scenario_guid = me.scenario_guid;
            assert_eq!(fresh, me, "{path}");
            let info = install.read(&path.replace(".a7me", ".a7minfo")).unwrap();
            assert_eq!(
                A7mInfo::parse(&info).unwrap().map_size,
                me.dimensions,
                "{path}"
            );
        }
    }

    fn unpack(grid: &ActiveMap) -> Vec<bool> {
        let (w, h) = (grid.width as usize, grid.height as usize);
        let stride = w.div_ceil(32) * 4;
        (0..w * h)
            .map(|i| grid.bits[i / w * stride + i % w / 8] >> (i % w % 8) & 1 != 0)
            .collect()
    }

    #[test]
    fn new_island_header_decodes() {
        let mut info = A7mInfo::new(
            [192, 192],
            "data/base/provinces/roman/islands/pool/my_island/my_island.a7me",
        );
        let mut cells = vec![false; 24 * 24];
        cells[2 * 24 + 3] = true;
        cells[10 * 24 + 20] = true;
        info.set_active_map(24, 24, &cells).unwrap();
        assert_eq!(info.active_rect, [24, 16, 168, 88]);
        info.start_coast_direction = Some(1.5);
        let bytes = info.encode();
        let back = A7mInfo::parse(&bytes).unwrap();
        assert_eq!(back.map_size, [192, 192]);
        assert_eq!(back.active_map, info.active_map);
        assert_eq!(back.start_coast_direction, Some(1.5));
        assert_eq!(back.versions.len(), 9);
        assert!(back.source_map.starts_with("data\\base\\"));
        // Ids follow write order, as in shipped files.
        let adf = Adf::decode(&bytes).unwrap();
        let size = adf.child_value(adf.root(), "MapSize").unwrap();
        assert_eq!(adf.record_id(size), 0x8001);
        let grid = adf.child_node(adf.root(), "ActiveMapGrid").unwrap();
        assert_eq!(adf.record_id(grid), 2);

        let me = A7me::new([192, 192]);
        assert_eq!(me.chunks.len(), 3);
        assert_eq!(A7me::parse(&me.encode()).unwrap(), me);
    }
}
