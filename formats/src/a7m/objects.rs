// The objects 117's and 1800's islands place, as their gamedata records
// them: buildings, units, quest objects, editor decorations and
// object-props, in one nested ADF per area inside
// `GameSessionManager/AreaManagerData`. This module decodes the fields the
// editor edits and writes edits back without touching anything else; the
// editor's own object model is core's (`anno_core::island::objects`).

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use anyhow::{Context, Result, ensure};

use crate::adf::{Adf, ItemId, utf16_bytes};

const AREA_DATA_PATH: &str = "GameSessionManager/AreaManagerData";
const LABEL_MAP_PATH: &str = "GameSessionManager/GameObjectManager/GameObjectLabelMap";

/// The `AreaObjectManager` list a record is stored in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Section {
    /// Session objects: buildings, units, slots, markers, audio spots, and
    /// on shipped islands mostly `SimpleCfg` decoration with a
    /// [`Object::config`] model (vegetation, decals, kit pieces).
    GameObject,
    /// Always empty on shipped islands.
    NaturePreset,
    /// Editor-layer decoration (mostly `SimpleCfg` + config), coastlines,
    /// rivers, lakes, herds.
    EditorObject,
    EditorOnlyObject,
    /// `Prop`-template objects: a `Prop` transform and no `Mesh`.
    Prop,
}

impl Section {
    /// The lists, in file order.
    const ALL: [Section; 5] = [
        Section::GameObject,
        Section::NaturePreset,
        Section::EditorObject,
        Section::EditorOnlyObject,
        Section::Prop,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Section::GameObject => "GameObject",
            Section::NaturePreset => "NaturePreset",
            Section::EditorObject => "EditorObject",
            Section::EditorOnlyObject => "EditorOnlyObject",
            Section::Prop => "Prop",
        }
    }
}

/// Where a record comes from on write. Opaque: it names entries of the
/// [`Areas`] it was read with.
#[derive(Debug, Clone)]
pub struct Origin(Entry);

#[derive(Debug, Clone)]
enum Entry {
    /// Loaded record: area index, entry under `objects`, original section.
    Loaded(usize, ItemId, Section),
    /// New record deep-copied from a loaded one, then overwritten.
    CopyOf(usize, ItemId),
    /// New record deep-copied from another island's area, then overwritten.
    Foreign(Arc<Adf>, ItemId),
}

impl Origin {
    /// The origin of a new record modelled on this one: a loaded record is
    /// copied, a copy or another island's record stays what it copies.
    pub fn copied(&self) -> Origin {
        match self.0 {
            Entry::Loaded(a, e, _) => Origin(Entry::CopyOf(a, e)),
            ref other => Origin(other.clone()),
        }
    }

    /// Whether the record stands in the file as loaded, rather than being
    /// a new copy.
    pub fn is_loaded(&self) -> bool {
        matches!(self.0, Entry::Loaded(..))
    }
}

impl PartialEq for Origin {
    fn eq(&self, other: &Self) -> bool {
        match (&self.0, &other.0) {
            (Entry::Loaded(a, b, c), Entry::Loaded(x, y, z)) => (a, b, c) == (x, y, z),
            (Entry::CopyOf(a, b), Entry::CopyOf(x, y)) => (a, b) == (x, y),
            (Entry::Foreign(a, b), Entry::Foreign(x, y)) => Arc::ptr_eq(a, x) && b == y,
            _ => false,
        }
    }
}

/// One control point of a `BezierPath`: island metres like
/// [`Object::position`]. Which other fields a point has depends on the
/// object; absent ones stay absent on save.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct PathPoint {
    pub p: [f32; 3],
    /// Coastlines: seaward direction.
    pub d: Option<[f32; 2]>,
    /// Bezier in and out tangents relative to `p`: rivers, lakes, work
    /// areas (whose two points run from the work area to its slot).
    pub i: Option<[f32; 3]>,
    pub o: Option<[f32; 3]>,
    /// Rivers: width in metres.
    pub w: Option<f32>,
    /// Rivers: three floats, meaning not decoded (often `1, ~3, 1`).
    pub u0: Option<[f32; 3]>,
}

/// A path through `points` with Bezier handles by the engine's BezierPath
/// rule, `scale` its `TangentScale` (1/6 for 117's paths, 0.3 for 1800's
/// work areas): an inner point's handles run along `next - prev`, `i`
/// scaled to `scale` times the chord before it and `o` the chord after,
/// each no longer than `|next - prev|`; an end point's `o` is
/// `scale * (next - p)` and `i = -o`. The game rebuilds them the same way
/// on load.
pub fn bezier_handles(points: &[[f32; 3]], scale: f32) -> Vec<PathPoint> {
    let n = points.len();
    type Vec3 = [f32; 3];
    let sub = |a: Vec3, b: Vec3| -> Vec3 { std::array::from_fn(|k| a[k] - b[k]) };
    let len = |v: Vec3| (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt();
    let capped = |v: Vec3, max: f32| {
        let l = len(v);
        if l > max && l > 0. {
            v.map(|x| x / l * max)
        } else {
            v
        }
    };
    (0..n)
        .map(|k| {
            let p = points[k];
            let (i, o) = match (
                k.checked_sub(1).map(|j| points[j]),
                points.get(k + 1).copied(),
            ) {
                (None, None) => ([0.; 3], [0.; 3]),
                (None, Some(next)) => {
                    let o = sub(next, p).map(|v| v * scale);
                    (o.map(|v| -v), o)
                }
                (Some(prev), None) => {
                    let o = sub(p, prev).map(|v| v * scale);
                    (o.map(|v| -v), o)
                }
                (Some(prev), Some(next)) => {
                    let d = sub(next, prev);
                    (
                        capped(d.map(|v| -v), len(sub(prev, p)) * scale),
                        capped(d, len(sub(next, p)) * scale),
                    )
                }
            };
            PathPoint {
                p,
                i: Some(i),
                o: Some(o),
                ..Default::default()
            }
        })
        .collect()
}

/// One placed object's record. Positions are island metres in the same
/// frame as [`crate::a7m::island::Prop`] positions and the height grid
/// (`x` = grid column, `z` = grid row, both times 2 for the 0.5 m height
/// texels; `y` = height).
#[derive(Debug, Clone, PartialEq)]
pub struct Object {
    /// `AreaManagerData` key of the area that stores the object (0x0001 is
    /// the main area; 0x2001, 0x4001 are further areas).
    pub area: u16,
    pub section: Section,
    /// Unique 64-bit object ID; see [`new_id`].
    pub id: u64,
    pub guid: u32,
    pub position: [f32; 3],
    /// Yaw in radians (`Direction`, 0 when absent).
    pub direction: f32,
    /// Full rotation as a quaternion (x, y, z, w): `Mesh/Orientation`, else
    /// `Prop/Rotation`, else derived from `direction`. Shipped yaw-only
    /// objects store `(0, -sin(d/2), 0, cos(d/2))`; terrain-aligned ones add
    /// tilt.
    pub rotation: [f32; 4],
    /// `Mesh/Scale` (uniform) or `Prop/Scale`; 1 when absent.
    pub scale: [f32; 3],
    /// Index into the asset's `cfgs` (`Variation`, 0 when absent).
    pub variation: u32,
    pub skin: u32,
    /// `OwnerProfile`: participant profile slot.
    pub owner: u32,
    /// Editor folder (`ObjectFolderID`, see `ObjectGroupFilterCollection`).
    pub folder: Option<u32>,
    /// `DynamicVariation/ConfigFilename`: a `.cfg` that overrides the
    /// asset's model. Stored with backslashes; `Install::file` accepts them.
    /// Generic assets such as `SimpleCfg` rely on it.
    pub config: Option<String>,
    /// `BezierPath/Path/BezierCurve` points, for objects with a path.
    pub path: Option<Vec<PathPoint>>,
    pub origin: Origin,
}

/// Yaw-only quaternion as the engine stores it (rotation by `-yaw` about +Y).
pub fn yaw_rotation(yaw: f32) -> [f32; 4] {
    let (s, c) = (yaw / 2.0).sin_cos();
    [0.0, -s, 0.0, c]
}

/// A fresh ID for a record modelled on the one with ID `like`: its high
/// word (editor tag, placing session, and bit 62 for props, which draw from
/// `NonGameObjectIDCounter`), and a low word one past the largest of `ids`
/// under that high word.
pub fn new_id(ids: impl IntoIterator<Item = u64>, like: u64) -> u64 {
    let high_word = |id: u64| id >> u32::BITS << u32::BITS;
    let high = high_word(like);
    let low = (ids.into_iter())
        .filter(|&id| high_word(id) == high)
        .map(|id| id as u32)
        .max()
        .map_or(1, |m| m + 1);
    high | u64::from(low)
}

#[derive(Debug, Clone)]
struct Area {
    key: u16,
    adf: Adf,
    sections: Vec<(Section, ItemId)>,
}

/// The areas of a gamedata tree's `AreaManagerData` as read: what a write
/// puts the edited records back into.
#[derive(Debug, Clone)]
pub struct Areas(Vec<Area>);

impl Areas {
    /// Decodes every area of a gamedata tree (`Island::gamedata()`) and the
    /// records they hold.
    pub fn read(gamedata: &Adf) -> Result<(Areas, Vec<Object>)> {
        let mut areas = Vec::new();
        let mut objects = Vec::new();
        for (key, _, data) in area_entries(gamedata)? {
            let adf = Adf::parse(gamedata.payload(data))
                .with_context(|| format!("area {key:#06x}: nested ADF"))?;
            let aom = adf
                .child_node(adf.root(), "AreaObjectManager")
                .with_context(|| format!("area {key:#06x}: no AreaObjectManager"))?;
            let mut sections = Vec::new();
            for section in Section::ALL {
                let Some(list) = adf.node(aom, &format!("{}/objects", section.name())) else {
                    continue;
                };
                sections.push((section, list));
                for entry in adf.children(list).filter(|&e| adf.is_node(e)) {
                    let o = decode_object(&adf, entry, key, section, areas.len())
                        .with_context(|| format!("area {key:#06x}: {} entry", section.name()))?;
                    objects.push(o);
                }
            }
            areas.push(Area { key, adf, sections });
        }
        Ok((Areas(areas), objects))
    }

    /// The float fields of one component of the record at `origin`
    /// (`River`, `Lake` …) by name, each as many floats as its payload
    /// holds. Empty for components the record lacks.
    pub fn component_floats(&self, origin: &Origin, component: &str) -> Vec<(String, Vec<f32>)> {
        let (adf, entry) = match &origin.0 {
            Entry::Loaded(a, e, _) | Entry::CopyOf(a, e) => (&self.0[*a].adf, *e),
            Entry::Foreign(adf, e) => (adf.as_ref(), *e),
        };
        let Some(node) = adf.node(entry, component) else {
            return Vec::new();
        };
        adf.children(node)
            .filter(|&c| !adf.is_node(c))
            .filter_map(|c| {
                let p = adf.payload(c);
                (!p.is_empty() && p.len().is_multiple_of(4)).then(|| {
                    let floats = p.as_chunks::<4>().0.iter().map(|b| f32::from_le_bytes(*b));
                    (adf.name(c).unwrap_or_default().to_owned(), floats.collect())
                })
            })
            .collect()
    }

    /// The loaded record at `origin` as an origin that carries it to another
    /// island: new records there copy it.
    pub fn carried(&self, origin: &Origin) -> Origin {
        let (area, entry) = match origin.0 {
            Entry::Loaded(a, e, _) | Entry::CopyOf(a, e) => (a, e),
            Entry::Foreign(..) => panic!("only this island's records are carried"),
        };
        Origin(Entry::Foreign(Arc::new(self.0[area].adf.clone()), entry))
    }

    /// Writes `objects`, the records read with these areas as edited (moved,
    /// turned, removed, copied), into `gamedata` (the tree they were read
    /// from, or an unedited copy of it). Areas without changes are left
    /// untouched, so an unedited write is byte-identical. Deleted IDs are
    /// also dropped from the object groups and `GameObjectLabelMap`.
    pub fn write(&self, objects: &[Object], gamedata: &mut Adf) -> Result<()> {
        let mut seen = HashSet::new();
        for o in objects {
            ensure!(seen.insert(o.id), "duplicate object id {:#x}", o.id);
            ensure!(
                self.0.iter().any(|a| a.key == o.area),
                "object {:#x}: no area {:#06x}",
                o.id,
                o.area
            );
        }
        let kept: HashSet<(usize, ItemId)> = objects
            .iter()
            .filter_map(|o| match o.origin.0 {
                Entry::Loaded(a, e, s) if self.0[a].key == o.area && s == o.section => Some((a, e)),
                _ => None,
            })
            .collect();
        let mut deleted_ids = HashSet::new();
        for (ai, area) in self.0.iter().enumerate() {
            for &(_, list) in &area.sections {
                for e in area.adf.children(list).filter(|&e| area.adf.is_node(e)) {
                    if !kept.contains(&(ai, e)) {
                        let id = area.adf.read_u64(
                            area.adf.child_value(e, "ID").context("object without ID")?,
                        )?;
                        if !seen.contains(&id) {
                            deleted_ids.insert(id);
                        }
                    }
                }
            }
        }

        let targets: HashMap<u16, (ItemId, ItemId)> = area_entries(gamedata)?
            .into_iter()
            .map(|(k, e, d)| (k, (e, d)))
            .collect();
        for (ai, area) in self.0.iter().enumerate() {
            let mut adf = area.adf.clone();
            let mut changed = false;
            // Deletions and moves out of this area.
            for &(_, list) in &area.sections {
                let entries: Vec<ItemId> = adf.children(list).filter(|&e| adf.is_node(e)).collect();
                for e in entries {
                    if !kept.contains(&(ai, e)) {
                        adf.remove(list, e);
                        changed = true;
                    }
                }
            }
            for o in objects.iter().filter(|o| o.area == area.key) {
                let list = area
                    .sections
                    .iter()
                    .find(|(s, _)| *s == o.section)
                    .map(|&(_, l)| l)
                    .with_context(|| {
                        format!("area {:#06x} has no {} list", area.key, o.section.name())
                    })?;
                let entry = match &o.origin.0 {
                    &Entry::Loaded(a, e, s) if a == ai && s == o.section => e,
                    &Entry::Loaded(a, e, _) | &Entry::CopyOf(a, e) => {
                        changed = true;
                        if a == ai {
                            copy_subtree(&area.adf, e, &mut adf, list)?
                        } else {
                            copy_subtree(&self.0[a].adf, e, &mut adf, list)?
                        }
                    }
                    Entry::Foreign(src, e) => {
                        changed = true;
                        copy_subtree(src, *e, &mut adf, list)?
                    }
                };
                changed |= write_fields(&mut adf, entry, o)?;
            }
            if !deleted_ids.is_empty() {
                changed |= drop_group_refs(&mut adf, &deleted_ids);
            }
            if !changed {
                continue;
            }
            let (entry, data) = *targets
                .get(&area.key)
                .with_context(|| format!("gamedata has no area {:#06x}", area.key))?;
            let bytes = adf.encode();
            gamedata.set_payload(data, &bytes);
            if let Some(count) = gamedata.child_value(entry, "ByteCount") {
                gamedata.set_payload(count, &(bytes.len() as u32).to_le_bytes());
            }
        }
        if !deleted_ids.is_empty()
            && let Some(labels) = gamedata.node(gamedata.root(), LABEL_MAP_PATH)
        {
            drop_labels(gamedata, labels, &deleted_ids);
        }
        Ok(())
    }
}

// `AreaManagerData` alternates an unnamed u16 area key with an unnamed node
// holding `ByteCount` and `Data` (the nested area ADF).
fn area_entries(gd: &Adf) -> Result<Vec<(u16, ItemId, ItemId)>> {
    let amd = gd
        .node(gd.root(), AREA_DATA_PATH)
        .with_context(|| format!("gamedata: no {AREA_DATA_PATH}"))?;
    let mut out = Vec::new();
    let mut key = None;
    for c in gd.children(amd) {
        if !gd.is_node(c) {
            key = Some(gd.read_u16(c).context("area key")?);
            continue;
        }
        let k = key.take().context("AreaManagerData: area without key")?;
        let data = gd
            .child_value(c, "Data")
            .with_context(|| format!("area {k:#06x}: no Data"))?;
        out.push((k, c, data));
    }
    Ok(out)
}

fn decode_object(
    adf: &Adf,
    e: ItemId,
    area: u16,
    section: Section,
    area_index: usize,
) -> Result<Object> {
    let u32_or = |name: &str, default: u32| -> Result<u32> {
        adf.child_value(e, name)
            .map_or(Ok(default), |v| adf.read_u32(v))
    };
    let id = adf.read_u64(adf.child_value(e, "ID").context("no ID")?)?;
    // Elided at the origin: Anno 117's Egypt DLC ships deco EditorObjects
    // (guid 37676) with no Position at all.
    let position = adf
        .child_value(e, "Position")
        .map_or(Ok([0.0; 3]), |v| adf.read_f32s(v))?;
    let direction = adf
        .child_value(e, "Direction")
        .map_or(Ok(0.0), |v| adf.read_f32(v))?;
    let rotation = match adf
        .value(e, "Mesh/Orientation")
        .or_else(|| adf.value(e, "Prop/Rotation"))
    {
        Some(v) => adf.read_f32s(v)?,
        None => yaw_rotation(direction),
    };
    let scale = if let Some(v) = adf.value(e, "Mesh/Scale") {
        [adf.read_f32(v)?; 3]
    } else if let Some(v) = adf.value(e, "Prop/Scale") {
        adf.read_f32s(v)?
    } else {
        [1.0; 3]
    };
    let config = adf
        .value(e, "DynamicVariation/ConfigFilename")
        .map(|v| adf.read_utf16(v))
        .transpose()?;
    Ok(Object {
        area,
        section,
        id,
        guid: u32_or("guid", 0)?,
        position,
        direction,
        rotation,
        scale,
        variation: u32_or("Variation", 0)?,
        skin: u32_or("SkinGuid", 0)?,
        owner: u32_or("OwnerProfile", 0)?,
        folder: adf
            .child_value(e, "ObjectFolderID")
            .map(|v| adf.read_u32(v))
            .transpose()?,
        config,
        path: decode_path(adf, e)?,
        origin: Origin(Entry::Loaded(area_index, e, section)),
    })
}

fn decode_path(adf: &Adf, e: ItemId) -> Result<Option<Vec<PathPoint>>> {
    let Some(curve) = adf.node(e, "BezierPath/Path/BezierCurve") else {
        return Ok(None);
    };
    adf.children(curve)
        .filter(|&c| adf.is_node(c))
        .map(|c| {
            let w = adf
                .child_value(c, "w")
                .map(|v| adf.read_f32s::<1>(v))
                .transpose()?
                .map(|[w]| w);
            Ok(PathPoint {
                p: adf.read_f32s(adf.child_value(c, "p").context("path point without p")?)?,
                d: adf
                    .child_value(c, "d")
                    .map(|v| adf.read_f32s(v))
                    .transpose()?,
                i: adf
                    .child_value(c, "i")
                    .map(|v| adf.read_f32s(v))
                    .transpose()?,
                o: adf
                    .child_value(c, "o")
                    .map(|v| adf.read_f32s(v))
                    .transpose()?,
                w,
                u0: adf
                    .child_value(c, "u0")
                    .map(|v| adf.read_f32s(v))
                    .transpose()?,
            })
        })
        .collect::<Result<_>>()
        .map(Some)
}

// Rewrites the curve and its bounds when they differ from the record.
fn write_path(adf: &mut Adf, e: ItemId, points: &[PathPoint]) -> Result<bool> {
    if decode_path(adf, e)?.as_deref() == Some(points) {
        return Ok(false);
    }
    let bp = match adf.child_node(e, "BezierPath") {
        Some(n) => n,
        None => adf.add_node(e, Some("BezierPath"))?,
    };
    let path = match adf.child_node(bp, "Path") {
        Some(n) => n,
        None => adf.add_node(bp, Some("Path"))?,
    };
    let (mut lo, mut hi) = ([f32::MAX; 3], [f32::MIN; 3]);
    for pt in points {
        for i in 0..3 {
            lo[i] = lo[i].min(pt.p[i]);
            hi[i] = hi[i].max(pt.p[i]);
        }
    }
    put(adf, path, "Minimum", &f32s_bytes(&lo), None)?;
    put(adf, path, "Maximum", &f32s_bytes(&hi), None)?;
    let curve = match adf.child_node(path, "BezierCurve") {
        Some(n) => {
            adf.clear_children(n);
            n
        }
        None => adf.add_node(path, Some("BezierCurve"))?,
    };
    for pt in points {
        let n = adf.add_node(curve, None)?;
        // Shipped field order.
        adf.add_value(n, Some("p"), &f32s_bytes(&pt.p))?;
        if let Some(d) = pt.d {
            adf.add_value(n, Some("d"), &f32s_bytes(&d))?;
        }
        if let Some(i) = pt.i {
            adf.add_value(n, Some("i"), &f32s_bytes(&i))?;
        }
        if let Some(o) = pt.o {
            adf.add_value(n, Some("o"), &f32s_bytes(&o))?;
        }
        if let Some(w) = pt.w {
            adf.add_value(n, Some("w"), &f32s_bytes(&[w]))?;
        }
        if let Some(u0) = pt.u0 {
            adf.add_value(n, Some("u0"), &f32s_bytes(&u0))?;
        }
    }
    Ok(true)
}

fn f32s_bytes(v: &[f32]) -> Vec<u8> {
    v.iter().flat_map(|f| f.to_le_bytes()).collect()
}

// Sets `parent/name` to `bytes` if it differs. A missing value is added only
// when `bytes` differs from the field's elided default.
fn put(
    adf: &mut Adf,
    parent: ItemId,
    name: &str,
    bytes: &[u8],
    default: Option<&[u8]>,
) -> Result<bool> {
    match adf.child_value(parent, name) {
        Some(v) if adf.payload(v) == bytes => Ok(false),
        Some(v) => {
            adf.set_payload(v, bytes);
            Ok(true)
        }
        None if default == Some(bytes) => Ok(false),
        // TODO(alex): appended after the existing fields. The engine matches
        // fields by name hash; whether it cares about order is untested.
        None => adf.add_value(parent, Some(name), bytes).map(|_| true),
    }
}

// Writes the editable fields; returns whether anything changed.
fn write_fields(adf: &mut Adf, e: ItemId, o: &Object) -> Result<bool> {
    let mut changed = false;
    changed |= put(adf, e, "guid", &o.guid.to_le_bytes(), None)?;
    changed |= put(adf, e, "ID", &o.id.to_le_bytes(), None)?;
    changed |= put(
        adf,
        e,
        "Variation",
        &o.variation.to_le_bytes(),
        Some(&[0; 4]),
    )?;
    changed |= put(adf, e, "SkinGuid", &o.skin.to_le_bytes(), Some(&[0; 4]))?;
    if let Some(folder) = o.folder {
        changed |= put(adf, e, "ObjectFolderID", &folder.to_le_bytes(), None)?;
    }
    changed |= put(
        adf,
        e,
        "Position",
        &f32s_bytes(&o.position),
        Some(&f32s_bytes(&[0.; 3])),
    )?;
    changed |= put(
        adf,
        e,
        "Direction",
        &o.direction.to_le_bytes(),
        Some(&[0; 4]),
    )?;
    changed |= put(
        adf,
        e,
        "OwnerProfile",
        &o.owner.to_le_bytes(),
        Some(&[0; 4]),
    )?;
    if let Some(mesh) = adf.child_node(e, "Mesh") {
        changed |= put(adf, mesh, "Orientation", &f32s_bytes(&o.rotation), None)?;
        changed |= put(
            adf,
            mesh,
            "Scale",
            &o.scale[0].to_le_bytes(),
            Some(&1f32.to_le_bytes()),
        )?;
    }
    if let Some(prop) = adf.child_node(e, "Prop") {
        changed |= put(adf, prop, "Position", &f32s_bytes(&o.position), None)?;
        changed |= put(adf, prop, "Rotation", &f32s_bytes(&o.rotation), None)?;
        changed |= put(adf, prop, "Scale", &f32s_bytes(&o.scale), None)?;
    }
    match (&o.config, adf.value(e, "DynamicVariation/ConfigFilename")) {
        (Some(c), _) => {
            let dv = match adf.child_node(e, "DynamicVariation") {
                Some(n) => n,
                None => adf.add_node(e, Some("DynamicVariation"))?,
            };
            changed |= put(adf, dv, "ConfigFilename", &utf16_bytes(c), None)?;
        }
        (None, Some(v)) => {
            let dv = adf.child_node(e, "DynamicVariation").expect("parent of v");
            adf.remove(dv, v);
            changed = true;
        }
        (None, None) => {}
    }
    if let Some(points) = &o.path {
        changed |= write_path(adf, e, points)?;
    }
    Ok(changed)
}

fn copy_subtree(src: &Adf, item: ItemId, dst: &mut Adf, parent: ItemId) -> Result<ItemId> {
    let name = src.name(item);
    if !src.is_node(item) {
        return dst.add_value(parent, name, src.payload(item));
    }
    let node = dst.add_node(parent, name)?;
    for c in src.children(item) {
        copy_subtree(src, c, dst, node)?;
    }
    Ok(node)
}

// `ObjectGroupCollection/ObjectGroups` alternates a name value with a node
// whose `GameObjects` lists member IDs.
fn drop_group_refs(adf: &mut Adf, ids: &HashSet<u64>) -> bool {
    let Some(groups) = adf.node(
        adf.root(),
        "AreaObjectManager/ObjectGroupCollection/ObjectGroups",
    ) else {
        return false;
    };
    let mut changed = false;
    let members: Vec<ItemId> = adf
        .children(groups)
        .filter_map(|g| adf.child_node(g, "GameObjects"))
        .collect();
    for list in members {
        let dead: Vec<ItemId> = adf
            .children(list)
            .filter(|&v| adf.read_u64(v).is_ok_and(|id| ids.contains(&id)))
            .collect();
        for v in dead {
            changed |= adf.remove(list, v);
        }
    }
    changed
}

// `GameObjectLabelMap` alternates a label string with the labelled ID.
fn drop_labels(gd: &mut Adf, labels: ItemId, ids: &HashSet<u64>) {
    let items: Vec<ItemId> = gd.children(labels).collect();
    for pair in items.chunks(2) {
        if let [label, id] = *pair
            && gd.read_u64(id).is_ok_and(|id| ids.contains(&id))
        {
            gd.remove(labels, label);
            gd.remove(labels, id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The engine's tangents: ends a TangentScale of their chord, inner
    // points along next - prev, each side its own chord's share.
    #[test]
    fn handles_follow_the_engine() {
        let h = bezier_handles(&[[0., 0., 0.], [3., 0., 0.], [3., 0., 1.]], 0.3);
        let near = |a: [f32; 3], b: [f32; 3]| (0..3).all(|k| (a[k] - b[k]).abs() < 1e-5);
        assert!(near(h[0].o.unwrap(), [0.9, 0., 0.]) && near(h[0].i.unwrap(), [-0.9, 0., 0.]));
        let d = [3., 0., 1.].map(|v: f32| v / 10f32.sqrt());
        let o = h[1].o.unwrap();
        let i = h[1].i.unwrap();
        assert!((0..3).all(|k| (o[k] - d[k] * 0.3).abs() < 1e-5), "{o:?}");
        assert!((0..3).all(|k| (i[k] + d[k] * 0.9).abs() < 1e-5), "{i:?}");
    }
    use crate::a7m::island::Island;
    use crate::assets::Assets;
    use crate::rda::Archive;
    use crate::vfs::Install;

    const PIRATE: &str = "data/base/provinces/roman/islands/3rdparty/roman_island_3rdparty_pirate_01/roman_island_3rdparty_pirate_01.a7m";
    const ISLANDS: [&str; 4] = [
        PIRATE,
        "data/base/provinces/roman/islands/pool/roman_island_large_09/roman_island_large_09.a7m",
        "data/base/provinces/celtic/islands/3rdparty/celtic_island_3rdparty_pirate_01/celtic_island_3rdparty_pirate_01.a7m",
        "data/dlc01/provinces/roman/islands/pool/roman_dlc01_island_continental_01/roman_dlc01_island_continental_01.a7m",
    ];

    fn install() -> Option<Install> {
        let Some(dir) = crate::vfs::install_dir("ANNO117_DIR") else {
            eprintln!("skip: ANNO117_DIR unset");
            return None;
        };
        Some(Install::open(dir, crate::vfs::Install::DATA_FIRST).unwrap())
    }

    fn members(install: &Install, path: &str) -> (Vec<u8>, Vec<u8>) {
        let a7m = Archive::parse(install.read(path).unwrap(), path).unwrap();
        (
            a7m.read_path("gamedata.data").unwrap(),
            a7m.read_path("rd3d.data").unwrap(),
        )
    }

    fn bits(v: [u32; 4]) -> [f32; 4] {
        v.map(f32::from_bits)
    }

    fn close(a: [f32; 4], b: [f32; 4]) -> bool {
        a.iter().zip(&b).all(|(x, y)| (x - y).abs() < 1e-4)
    }

    #[test]
    fn yaw_rotation_matches_shipped_orientation() {
        // pirate_01 area 0x0001, first GameObject: Direction 0x40971f61,
        // Mesh/Orientation as stored.
        let q = bits([0, 0xbf34_180d, 0, 0xbf35_f051]);
        assert!(close(yaw_rotation(f32::from_bits(0x4097_1f61)), q));
    }

    #[test]
    fn decodes_pirate_objects_inside_the_island() {
        let Some(install) = install() else { return };
        let (gd, rd) = members(&install, PIRATE);
        let island = Island::parse(&gd, &rd).unwrap();
        let (areas, objects) = Areas::read(island.gamedata()).unwrap();
        assert_eq!(
            areas.0.iter().map(|a| a.key).collect::<Vec<_>>(),
            [0x2001, 0x0001]
        );
        let count = |area: u16, s: Section| {
            objects
                .iter()
                .filter(|o| o.area == area && o.section == s)
                .count()
        };
        assert_eq!(count(0x2001, Section::GameObject), 56);
        assert_eq!(count(0x0001, Section::GameObject), 806);
        assert_eq!(count(0x0001, Section::EditorObject), 2525);
        assert_eq!(count(0x0001, Section::Prop), 324);

        let [w, h] = island.world_size().map(|v| v as f32);
        let heights = island.heights();
        let mut near_ground = 0;
        for o in &objects {
            let [x, y, z] = o.position;
            assert!((0.0..w).contains(&x) && (0.0..h).contains(&z), "{o:?}");
            let ground = heights.get((x * 2.0).round() as usize, (z * 2.0).round() as usize);
            if (y - ground).abs() < 1.0 {
                near_ground += 1;
            }
            let n: f32 = o.rotation.iter().map(|v| v * v).sum();
            assert!((n - 1.0).abs() < 1e-3, "{o:?}");
        }
        // Most objects stand on the terrain; units on walls and roofs do not.
        assert!(near_ground * 10 > objects.len() * 7, "{near_ground}");

        let tower = objects.iter().find(|o| o.guid == 87583).unwrap();
        assert_eq!(tower.section, Section::GameObject);
        assert!(
            objects
                .iter()
                .any(|o| o.config.as_deref().is_some_and(|c| c.ends_with(".cfg")))
        );
    }

    #[test]
    fn placed_objects_resolve_to_assets() {
        let Some(install) = install() else { return };
        let schema = crate::assets::Schema {
            assets_path: "data/base/config/export/assets.xml",
            ..crate::assets::Schema::FIXTURE
        };
        let assets = Assets::load(&install, &schema).unwrap();
        let (gd, _) = members(&install, PIRATE);
        let (_, objects) = Areas::read(&Adf::parse(&gd).unwrap()).unwrap();
        let mut drawn = 0;
        for o in &objects {
            let asset = assets.get(o.guid);
            assert!(asset.is_some(), "unknown guid {}", o.guid);
            let cfgs = &asset.unwrap().cfgs;
            if !cfgs.is_empty() && o.config.is_none() {
                assert!((o.variation as usize) < cfgs.len(), "{o:?} {cfgs:?}");
            }
            // A model to draw: the record's override, else the asset's.
            if o.config.is_some() || !cfgs.is_empty() {
                drawn += 1;
            }
        }
        assert!(drawn * 10 > objects.len() * 8, "{drawn}");
    }

    #[test]
    fn unedited_write_is_byte_identical() {
        let Some(install) = install() else { return };
        for path in ISLANDS {
            let (bytes, _) = members(&install, path);
            let gd = Adf::parse(&bytes).unwrap();
            let (areas, objects) = Areas::read(&gd).unwrap();
            let mut out = gd.clone();
            areas.write(&objects, &mut out).unwrap();
            assert!(out.encode() == bytes, "{path}");
        }
    }

    #[test]
    fn edits_survive_a_reread() {
        let Some(install) = install() else { return };
        let (bytes, rd) = members(&install, PIRATE);
        let gd = Adf::parse(&bytes).unwrap();
        let (areas, mut objects) = Areas::read(&gd).unwrap();
        let before = objects.len();

        let moved = objects
            .iter()
            .position(|o| o.section == Section::GameObject && o.area == 1)
            .unwrap();
        let moved_id = objects[moved].id;
        let mut target = objects[moved].position;
        target[0] += 3.0;
        objects[moved].position = target;
        objects[moved].direction = 1.25;
        objects[moved].rotation = yaw_rotation(1.25);

        // An object that sits in an object group, so the group ref goes too.
        let area = &areas.0[1].adf;
        let grouped: Vec<u64> = area
            .children(
                area.node(
                    area.root(),
                    "AreaObjectManager/ObjectGroupCollection/ObjectGroups",
                )
                .unwrap(),
            )
            .filter_map(|g| area.child_node(g, "GameObjects"))
            .flat_map(|l| area.children(l).map(|v| area.read_u64(v).unwrap()))
            .collect();
        let deleted = objects
            .iter()
            .position(|o| o.id != moved_id && grouped.contains(&o.id))
            .unwrap();
        let deleted_id = objects.remove(deleted).id;

        // A copy of the first record, under a fresh id.
        let copy_id = new_id(objects.iter().map(|o| o.id), objects[0].id);
        assert!(objects.iter().all(|o| o.id != copy_id));
        let mut copy = Object {
            id: copy_id,
            origin: objects[0].origin.copied(),
            ..objects[0].clone()
        };
        copy.position[2] += 5.0;
        objects.push(copy);

        let mut out = gd.clone();
        areas.write(&objects, &mut out).unwrap();
        let encoded = out.encode();
        let (_, again) = Areas::read(&Adf::parse(&encoded).unwrap()).unwrap();
        assert_eq!(again.len(), before);
        let find = |id: u64| again.iter().find(|o| o.id == id);
        let m = find(moved_id).unwrap();
        assert_eq!(m.position, target);
        assert_eq!(m.direction, 1.25);
        assert!(close(m.rotation, yaw_rotation(1.25)));
        assert!(find(deleted_id).is_none());
        assert_eq!(find(copy_id).unwrap().guid, objects[0].guid);
        assert!(!encoded.windows(8).any(|w| w == deleted_id.to_le_bytes()));

        // The rest of the island still loads.
        Island::parse(&encoded, &rd).unwrap();
    }
}
