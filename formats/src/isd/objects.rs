// The objects an island places (`m_GOPManager/Objects`), record by record:
// where each list and record stands, what a record holds, the field writes
// that change one in place, and the ids and links that tie records
// together. Which document object or prop a record becomes, and which
// record a new one copies, are the game's.

use anyhow::{Context, Result, bail, ensure};

use super::{Doc, FIXED, NodeId, Value};

// `m_Position`: x, z, then height, each a little-endian i64 in fixed-point
// metres.
const POSITION: usize = 3 * size_of::<i64>();
// `Nature/m_Orientation`: an `(x, y, z, w)` quaternion of f32.
const ORIENTATION: usize = 4 * size_of::<f32>();
// `ObjectGroups`: each group a blob of little-endian u32 object ids.
const GROUP_ID: usize = size_of::<u32>();

// The transform the renderer draws a record at, as `m_RenderParameters`
// holds it: a block of position, rotation and a uniform scale, then a
// colour, ending the blob. Shipped records hold three forms, by length:
// 69 bytes (the block at 21), 78 (at 30) and 130 (at 82, with a second
// copy at 30 that the renderer also reads). One shipped record's blocks
// stand elsewhere, and none is found there.
struct RenderBlock;

impl RenderBlock {
    const LEN: usize = 48;
    const POSITION: std::ops::Range<usize> = 0..12;
    const ROTATION: std::ops::Range<usize> = 12..28;
    const SCALE: std::ops::Range<usize> = 28..32;
    // What a write changes: position, rotation and scale.
    const TRANSFORM: std::ops::Range<usize> = 0..32;
    // (blob length, where it holds a second copy of the block).
    const SECOND_COPY: (usize, usize) = (130, 30);
}

/// The list an object sits in, in file order. Buildings and harbours are
/// handles; nature is trees and rocks; grass is ground cover.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Section {
    Handle,
    Feedback,
    Simple,
    Nature,
    Grass,
}

const SECTIONS: [(&str, Section); 5] = [
    ("Handle", Section::Handle),
    ("Feedback", Section::Feedback),
    ("Simple", Section::Simple),
    ("Nature", Section::Nature),
    ("Grass", Section::Grass),
];

impl Section {
    /// Every list, in file order.
    pub const ALL: [Section; 5] = [
        Section::Handle,
        Section::Feedback,
        Section::Simple,
        Section::Nature,
        Section::Grass,
    ];

    fn name(self) -> &'static str {
        SECTIONS[self as usize].0
    }

    // Trees, rocks and grass keep their rotation and scale in a `Nature`
    // child; the others in their render parameters.
    fn nature(self) -> bool {
        matches!(self, Section::Nature | Section::Grass)
    }
}

/// One placed object as its record stores it.
#[derive(Clone, Debug, PartialEq)]
pub struct Object {
    pub section: Section,
    pub id: u64,
    pub guid: u32,
    /// Index into the asset's models (`m_Variation`, 0 when absent). A
    /// few scenario records store -1.
    pub variation: i32,
    /// World metres: x, height, z.
    pub position: [f32; 3],
    /// `m_Direction`, [`QUARTER_TURN`](Self::QUARTER_TURN) per quarter turn.
    pub direction: i64,
    /// `(x, y, z, w)`.
    pub rotation: [f32; 4],
    /// `Nature/m_Scale`, or the render parameters' scale; 1 when absent.
    pub scale: f32,
    /// `m_PlayerID`, absent on about 12,000 shipped records.
    pub player: Option<u32>,
}

/// Every object `doc` places, section by section in file order.
pub fn read(doc: &Doc, file: &str) -> Result<Vec<Object>> {
    (records(doc).into_iter().enumerate())
        .map(|(k, (section, o))| {
            Object::read(doc, o, section).with_context(|| format!("{file}: {section:?} object {k}"))
        })
        .collect()
}

/// `section`'s `Objects` node; `None` for a list the file lacks.
pub fn list(doc: &Doc, section: Section) -> Option<NodeId> {
    let objects = (doc.root("m_GOPManager")).and_then(|g| doc.child(g, "Objects"))?;
    doc.child(doc.child(objects, section.name())?, "Objects")
}

/// Where each record [`read`] reads stands, in its order.
pub fn records(doc: &Doc) -> Vec<(Section, NodeId)> {
    (Section::ALL.into_iter())
        .filter_map(|section| Some((section, list(doc, section)?)))
        .flat_map(|(section, list)| doc.children(list).map(move |o| (section, o)))
        .collect()
}

/// The record a one-record document holds ([`excerpt`]), read as one of
/// `section`.
pub fn read_record(doc: &Doc, section: Section) -> Result<Object> {
    let root = doc.roots().next().context("an empty record")?;
    Object::read(doc, root, section)
}

/// Record `o` of `doc` alone, as a document of one `Object` element: what
/// a record copied from another island travels as.
pub fn excerpt(doc: &Doc, o: NodeId) -> Result<Vec<u8>> {
    let mut one = Doc::parse(b"<Object></Object>\r\n")?;
    let root = one.roots().next().expect("one element");
    for c in doc.children(o) {
        copy_into(doc, c, &mut one, root)?;
    }
    Ok(one.encode())
}

/// Appends the record `record`, a one-record document ([`excerpt`]),
/// at the end of `section`'s list in `doc`; gives the copy.
pub fn append_record(doc: &mut Doc, section: Section, record: &Doc) -> Result<NodeId> {
    let list = list(doc, section).with_context(|| format!("no {section:?} list"))?;
    let root = record.roots().next().context("an empty record")?;
    copy_into(record, root, doc, list)
}

/// Adds `section`'s list, empty, under `m_GOPManager/Objects`, before the
/// lists that follow it. 183 shipped islands have no `Grass`.
pub fn add_list(doc: &mut Doc, section: Section) -> Result<()> {
    let objects = (doc.root("m_GOPManager"))
        .and_then(|g| doc.child(g, "Objects"))
        .context("no m_GOPManager/Objects")?;
    let list = match doc.child(objects, section.name()) {
        Some(l) => l,
        None => {
            let after = &SECTIONS[section as usize + 1..];
            let before = after.iter().find_map(|(name, _)| doc.child(objects, name));
            doc.insert(objects, before, section.name())?
        }
    };
    doc.insert(list, None, "Objects")?;
    Ok(())
}

// Copies `node`'s element of `src` to the end of `parent`'s children in
// `out`, as `out`'s own nodes; gives the copy.
fn copy_into(src: &Doc, node: NodeId, out: &mut Doc, parent: NodeId) -> Result<NodeId> {
    let name = std::str::from_utf8(src.name(node))?;
    let copy = out.insert(parent, None, name)?;
    match src.value(node) {
        Value::Empty => {}
        Value::Text(t) => out.set(copy, t, false)?,
        Value::Blob(b) => out.set(copy, b, true)?,
        // Children go at the copy's end; the copy itself stays put.
        Value::Children => {
            for c in src.children(node) {
                copy_into(src, c, out, copy)?;
            }
        }
    }
    Ok(copy)
}

impl Object {
    /// `m_Direction` of a quarter turn.
    pub const QUARTER_TURN: i64 = 368_640;

    /// Reads record `o` of list `section`.
    ///
    /// A record holds `m_ID`, `m_GUID`, an optional `m_Variation`,
    /// `m_Position` (three little-endian i64 in 1/4096 m: x, z, then
    /// height), an optional `m_PlayerID` and `m_Direction`. Ids are unique
    /// across all five lists of an island. Nature and grass add a `Nature`
    /// child with `m_Scale` and the whole rotation as an `(x, y, z, w)`
    /// quaternion, tilt included; for the yaw-only ones it is
    /// `(0, -sin(d/2), 0, cos(d/2))` of the direction. The others keep what
    /// the renderer draws in `PropertyMesh/m_RenderParameters`
    /// (`render_block`): position, rotation (tilted on about 3,400 shipped
    /// records) and scale (0.1 to 8 on about 450).
    pub fn read(doc: &Doc, o: NodeId, section: Section) -> Result<Object> {
        let text = |name: &str| -> Result<Option<&str>> {
            match doc.child(o, name).map(|c| doc.value(c)) {
                None => Ok(None),
                Some(Value::Text(t)) => Ok(Some(std::str::from_utf8(t)?)),
                Some(_) => bail!("{name} is not text"),
            }
        };
        let blob = |at: NodeId, name: &str| -> Option<&[u8]> {
            match doc.value(doc.child(at, name)?) {
                Value::Blob(b) => Some(b),
                _ => None,
            }
        };
        fn number<T: std::str::FromStr>(name: &str, t: Option<&str>) -> Result<Option<T>> {
            t.map(|t| {
                t.trim()
                    .parse()
                    .map_err(|_| anyhow::anyhow!("{name} {t:?}"))
            })
            .transpose()
        }
        let id = number("m_ID", text("m_ID")?)?.context("no m_ID")?;
        let guid = number("m_GUID", text("m_GUID")?)?.context("no m_GUID")?;
        let variation = number("m_Variation", text("m_Variation")?)?.unwrap_or(0);
        let direction: i64 = number("m_Direction", text("m_Direction")?)?.unwrap_or(0);
        let player = number("m_PlayerID", text("m_PlayerID")?)?;
        let p = blob(o, "m_Position").context("no m_Position")?;
        ensure!(p.len() == POSITION, "m_Position of {} bytes", p.len());
        let (fixed, _) = p.as_chunks::<{ size_of::<i64>() }>();
        let [x, z, y] = [0, 1, 2].map(|k| i64::from_le_bytes(fixed[k]) as f32 / FIXED as f32);
        let position = [x, y, z];
        let nature = doc.child(o, "Nature");
        let drawn = (doc.child(o, "PropertyMesh"))
            .and_then(|m| blob(m, "m_RenderParameters"))
            .and_then(|r| render_block(r, position));
        let rotation = match (nature.and_then(|n| blob(n, "m_Orientation")), drawn) {
            (Some(q), _) => {
                ensure!(q.len() == ORIENTATION, "m_Orientation of {} bytes", q.len());
                floats(q)
            }
            (None, Some((_, q, _))) => q,
            (None, None) => yaw_rotation(yaw(direction)),
        };
        let scale = match nature
            .and_then(|n| doc.child(n, "m_Scale"))
            .map(|c| doc.value(c))
        {
            Some(Value::Text(t)) => std::str::from_utf8(t)?.parse()?,
            _ => drawn.map_or(1., |(_, _, s)| s),
        };
        Ok(Object {
            section,
            id,
            guid,
            variation,
            position,
            direction,
            rotation,
            scale,
            player,
        })
    }

    /// Radians of `m_Direction`.
    pub fn yaw(&self) -> f32 {
        yaw(self.direction)
    }

    /// `m_Direction` for `yaw` radians, or this record's own when that is
    /// the same turn: a move doesn't renumber 1,105,920 as -368,640.
    pub fn turned_to(&self, yaw: f32) -> i64 {
        let quarter = Self::QUARTER_TURN as f32;
        let d = (yaw / std::f32::consts::FRAC_PI_2 * quarter).round() as i64;
        let full = 4 * Self::QUARTER_TURN;
        match (d - self.direction).rem_euclid(full) {
            0 | 1 => self.direction,
            r if r == full - 1 => self.direction,
            _ => d,
        }
    }

    /// Writes `now` into record `o` of `doc`, which reads as this one,
    /// changing only the fields that differ:
    /// - `m_GUID`, and `m_Variation`, added where the record lacks it
    /// - `m_Position`, to the 1/4096 m a record stores
    /// - `m_Direction` (`now.direction`, see [`turned_to`](Self::turned_to))
    ///   and the rotation, when `now.rotation` turns away from this one;
    ///   turns under about 0.001 rad read as none, since a moved matrix
    ///   decomposes with its last bits loose
    /// - `m_PlayerID`, added or removed
    /// - the scale
    ///
    /// Trees, rocks and grass keep rotation and scale in `Nature` (a scale
    /// with six decimals, as shipped); the other lists in the render
    /// parameters' transform block, which changes only where it stands at
    /// the record's position.
    pub fn write(&self, doc: &mut Doc, o: NodeId, now: &Object) -> Result<()> {
        if now.guid != self.guid {
            set_text(doc, o, "m_GUID", &now.guid.to_string())?;
        }
        if now.variation != self.variation {
            put_text(
                doc,
                o,
                "m_Variation",
                &now.variation.to_string(),
                "m_Position",
            )?;
        }
        let at = stored(now.position);
        let moved = at.map(f32::to_bits) != self.position.map(f32::to_bits);
        if moved {
            let p = doc.child(o, "m_Position").context("no m_Position")?;
            doc.set(p, &position_bytes(at), true)?;
        }
        let turned = turned(self.rotation, now.rotation);
        if self.section.nature() && turned {
            let nature = doc.child(o, "Nature").context("no Nature")?;
            let n = (doc.child(nature, "m_Orientation")).context("no m_Orientation")?;
            doc.set(n, &now.rotation.map(f32::to_le_bytes).concat(), true)?;
        }
        if turned && now.direction != self.direction {
            set_text(doc, o, "m_Direction", &now.direction.to_string())?;
        }
        if now.player != self.player {
            match now.player {
                Some(p) => put_text(doc, o, "m_PlayerID", &p.to_string(), "m_Direction")?,
                None => {
                    if let Some(p) = doc.child(o, "m_PlayerID") {
                        doc.remove(p);
                    }
                }
            }
        }
        let scaled = scaled(self.scale, now.scale);
        if self.section.nature() {
            if scaled {
                let nature = doc.child(o, "Nature").context("no Nature")?;
                let scale = format!("{:.6}", now.scale);
                put_text(doc, nature, "m_Scale", &scale, "m_Orientation")?;
            }
            return Ok(());
        }
        let render =
            (doc.child(o, "PropertyMesh")).and_then(|m| doc.child(m, "m_RenderParameters"));
        if let Some(render) = render.filter(|_| moved || turned || scaled)
            && let Value::Blob(r) = doc.value(render)
            && let Some((block, _, _)) = render_block(r, self.position)
        {
            let mut r = r.to_vec();
            let transform =
                |at: usize| at + RenderBlock::TRANSFORM.start..at + RenderBlock::TRANSFORM.end;
            let old = r[transform(block)].to_vec();
            let mut new = old.clone();
            new[RenderBlock::POSITION].copy_from_slice(&at.map(f32::to_le_bytes).concat());
            if turned {
                let q = now.rotation.map(f32::to_le_bytes).concat();
                new[RenderBlock::ROTATION].copy_from_slice(&q);
            }
            if scaled {
                new[RenderBlock::SCALE].copy_from_slice(&now.scale.to_le_bytes());
            }
            r[transform(block)].copy_from_slice(&new);
            // The second copy, where the form has one and it holds the same.
            let (len, second) = RenderBlock::SECOND_COPY;
            if r.len() == len && block != second && r[transform(second)] == old[..] {
                r[transform(second)].copy_from_slice(&new);
            }
            doc.set(render, &r, true)?;
        }
        Ok(())
    }
}

/// Gives record `o`, a fresh copy, id `id`, and drops what it would share
/// with its original: a wall's child becomes 0, and `LinkedObjects` is
/// left empty. That element sits on buildings tied to parts they place
/// (wind and current parks, solar tower fields, repair docks, the Ecos
/// windpark prop); all 85 shipped records hold it empty, since the game
/// links the parts when it builds them, so a copy never claims its
/// original's parts.
pub fn renumber(doc: &mut Doc, o: NodeId, id: u64) -> Result<()> {
    set_text(doc, o, "m_ID", &id.to_string())?;
    let child = (doc.child(o, "Wall")).and_then(|w| doc.child(w, "m_ChildHID"));
    if let Some(c) = child.filter(|&c| text_number(doc, c) != Some(0)) {
        doc.set(c, b"0", false)?;
    }
    if let Some(l) = doc.child(o, "LinkedObjects") {
        match doc.value(l) {
            Value::Empty => {}
            Value::Children => {
                let inner: Vec<NodeId> = doc.children(l).collect();
                doc.remove_all(&inner);
            }
            Value::Text(_) | Value::Blob(_) => doc.set(l, b"", false)?,
        }
    }
    Ok(())
}

/// The largest object id `doc` uses: on a record of `loaded` (what
/// [`read`] gave), or named by a link, which may name an object the island
/// no longer has. One counter numbers all five lists, so a new record takes
/// one past it.
pub fn largest_id(doc: &Doc, loaded: &[Object]) -> u64 {
    let mut largest = loaded.iter().map(|o| o.id).max().unwrap_or(0);
    let mut see = |v: i64| largest = largest.max(u64::try_from(v).unwrap_or(0));
    let Some(gop) = doc.root("m_GOPManager") else {
        return largest;
    };
    for name in ["SnapPartner", "ObjectNames"] {
        for n in (doc.child(gop, name))
            .into_iter()
            .flat_map(|l| doc.children(l))
        {
            if doc.name(n) == b"k" || name == "SnapPartner" {
                text_number(doc, n).map(&mut see);
            }
        }
    }
    for g in (doc.child(gop, "ObjectGroups"))
        .into_iter()
        .flat_map(|l| doc.children(l))
    {
        if let Value::Blob(b) = doc.value(g) {
            (b.as_chunks::<GROUP_ID>().0.iter()).for_each(|&w| see(u32::from_le_bytes(w).into()));
        }
    }
    for (section, o) in records(doc) {
        wall_child(doc, section, o)
            .and_then(|c| text_number(doc, c))
            .map(&mut see);
    }
    largest
}

/// Drops the links that name an id in `gone` (sorted): name and snap
/// partner pairs, the ids in object groups (a group left empty goes), and
/// a wall's child, which becomes 0 as on walls without one. Shipped walls
/// name 67 children that aren't on their island, so a missing child is
/// survivable; a link out of the other lists may not be. The nodes to
/// remove join `removed`, so they go in the caller's one pass.
pub fn unlink(doc: &mut Doc, gone: &[u64], removed: &mut Vec<NodeId>) -> Result<()> {
    if gone.is_empty() {
        return Ok(());
    }
    let is_gone = |v: Option<i64>| {
        v.and_then(|v| u64::try_from(v).ok())
            .is_some_and(|v| gone.binary_search(&v).is_ok())
    };
    for (section, o) in records(doc) {
        let child = wall_child(doc, section, o);
        if let Some(c) = child.filter(|&c| is_gone(text_number(doc, c))) {
            doc.set(c, b"0", false)?;
        }
    }
    let gop = doc.root("m_GOPManager").context("no m_GOPManager")?;
    // `k` then `v`: an object id and its name, or two snapped objects.
    for (name, both) in [("SnapPartner", true), ("ObjectNames", false)] {
        let Some(list) = doc.child(gop, name) else {
            continue;
        };
        let pairs: Vec<NodeId> = doc.children(list).collect();
        for pair in pairs.chunks(2) {
            let &[k, v] = pair else {
                bail!("{name}: a key without its value");
            };
            ensure!(
                doc.name(k) == b"k" && doc.name(v) == b"v",
                "{name}: pairs of k and v"
            );
            if is_gone(text_number(doc, k)) || (both && is_gone(text_number(doc, v))) {
                removed.extend([k, v]);
            }
        }
    }
    let Some(groups) = doc.child(gop, "ObjectGroups") else {
        return Ok(());
    };
    for g in doc.children(groups).collect::<Vec<_>>() {
        let Value::Blob(b) = doc.value(g) else {
            continue;
        };
        let kept: Vec<u8> = (b.as_chunks::<GROUP_ID>().0.iter())
            .filter(|&&w| !is_gone(Some(u32::from_le_bytes(w).into())))
            .flatten()
            .copied()
            .collect();
        if kept.len() == b.len() {
            continue;
        }
        if kept.is_empty() {
            removed.push(g);
        } else {
            doc.set(g, &kept, true)?;
        }
    }
    Ok(())
}

// A handle record's wall child (`Wall/m_ChildHID`).
fn wall_child(doc: &Doc, section: Section, o: NodeId) -> Option<NodeId> {
    (section == Section::Handle)
        .then(|| {
            doc.child(o, "Wall")
                .and_then(|w| doc.child(w, "m_ChildHID"))
        })
        .flatten()
}

fn floats<const N: usize>(b: &[u8]) -> [f32; N] {
    let (f, _) = b.as_chunks::<{ size_of::<f32>() }>();
    std::array::from_fn(|k| f32::from_le_bytes(f[k]))
}

// Radians of `m_Direction` `d`.
fn yaw(d: i64) -> f32 {
    d as f32 / Object::QUARTER_TURN as f32 * std::f32::consts::FRAC_PI_2
}

// The quaternion of a turn by `yaw` alone, as the engine reads one: a
// rotation by `-yaw` about +Y. What a record with neither an orientation
// nor a drawn transform turns by.
fn yaw_rotation(yaw: f32) -> [f32; 4] {
    let (s, c) = (yaw / 2.0).sin_cos();
    [0.0, -s, 0.0, c]
}

// Where `m_RenderParameters` `r` holds its `RenderBlock`, and the block's
// rotation and scale, when it stands at the record's `position`.
fn render_block(r: &[u8], position: [f32; 3]) -> Option<(usize, [f32; 4], f32)> {
    let at = r.len().checked_sub(RenderBlock::LEN)?;
    let block = &r[at..];
    let p: [f32; 3] = floats(&block[RenderBlock::POSITION]);
    (p.map(f32::to_bits) == position.map(f32::to_bits)).then(|| {
        let [scale] = floats(&block[RenderBlock::SCALE]);
        (at, floats(&block[RenderBlock::ROTATION]), scale)
    })
}

// `m_Position` of `p`.
fn position_bytes(p: [f32; 3]) -> Vec<u8> {
    [p[0], p[2], p[1]]
        .map(|v| ((v * FIXED as f32).round() as i64).to_le_bytes())
        .concat()
}

// `p` as a record stores it, to the fixed point. The renderer's copy takes
// the same values, so a reread finds it standing at the record.
fn stored(p: [f32; 3]) -> [f32; 3] {
    p.map(|v| (v * FIXED as f32).round() / FIXED as f32)
}

// Whether `b` turns away from `a`.
fn turned(a: [f32; 4], b: [f32; 4]) -> bool {
    let dot: f32 = (0..4).map(|k| a[k] * b[k]).sum();
    dot.abs() < 1. - 5e-7
}

fn scaled(a: f32, b: f32) -> bool {
    (a - b).abs() > 1e-6 * a.abs().max(1.)
}

fn set_text(doc: &mut Doc, o: NodeId, name: &str, value: &str) -> Result<()> {
    let at = doc.child(o, name).with_context(|| format!("no {name}"))?;
    doc.set(at, value.as_bytes(), false)
}

// Sets `o`'s `name` to `value`, adding it before `before` when the record
// lacks it (shipped records order their fields the same way).
fn put_text(doc: &mut Doc, o: NodeId, name: &str, value: &str, before: &str) -> Result<()> {
    let at = match doc.child(o, name) {
        Some(at) => at,
        None => {
            let next = doc.child(o, before);
            doc.insert(o, next, name)?
        }
    };
    doc.set(at, value.as_bytes(), false)
}

fn text_number(doc: &Doc, n: NodeId) -> Option<i64> {
    match doc.value(n) {
        Value::Text(t) => std::str::from_utf8(t).ok()?.trim().parse().ok(),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn element(name: &str, inner: &[u8]) -> Vec<u8> {
        [
            format!("<{name}>").as_bytes(),
            inner,
            format!("</{name}>\r\n").as_bytes(),
        ]
        .concat()
    }

    fn blob(bytes: &[u8]) -> Vec<u8> {
        let len = u32::try_from(bytes.len()).unwrap().to_le_bytes();
        [super::super::CDATA, &len[..], bytes, b"]"].concat()
    }

    fn position(p: [f32; 3]) -> Vec<u8> {
        blob(&position_bytes(p))
    }

    fn handle(id: u64, at: [f32; 3], direction: i64) -> Vec<u8> {
        let fields = [
            element("m_ID", id.to_string().as_bytes()),
            element("m_GUID", b"335"),
            element("m_Position", &position(at)),
            element("m_Direction", direction.to_string().as_bytes()),
        ];
        element("Object", &fields.concat())
    }

    fn island(handles: &[u8], groups: &[u8]) -> Vec<u8> {
        let lists = element("Handle", &element("Objects", handles));
        element(
            "m_GOPManager",
            &[element("Objects", &lists), groups.to_vec()].concat(),
        )
    }

    // A record reads its fixed-point position (x, z, height on file) and its
    // turn; a write changes only the fields that differ, adding an owner
    // before the direction as shipped records order it.
    #[test]
    fn a_record_reads_its_fields_and_writes_only_what_changed() {
        let quarter = Object::QUARTER_TURN;
        let before = island(&handle(7, [1.5, 0.5, 2.25], quarter), b"");
        let mut doc = Doc::parse(&before).unwrap();
        let read = read(&doc, "isle.isd").unwrap();
        let [was] = &read[..] else { panic!("{read:?}") };
        assert_eq!((was.section, was.id, was.guid), (Section::Handle, 7, 335));
        assert_eq!(was.position, [1.5, 0.5, 2.25]);
        assert!((was.yaw() - std::f32::consts::FRAC_PI_2).abs() < 1e-6);
        assert_eq!(was.rotation, yaw_rotation(was.yaw()));
        // The same turn the other way round keeps its number.
        assert_eq!(was.turned_to(-3. * std::f32::consts::FRAC_PI_2), quarter);
        assert_eq!(was.turned_to(std::f32::consts::PI), 2 * quarter);

        let o = records(&doc)[0].1;
        was.write(&mut doc, o, was).unwrap();
        assert!(doc.encode() == before, "an unchanged record wrote");
        let now = Object {
            position: [3., 0.5, 2.25],
            player: Some(2),
            ..was.clone()
        };
        was.write(&mut doc, o, &now).unwrap();
        let fields = [
            element("m_ID", b"7"),
            element("m_GUID", b"335"),
            element("m_Position", &position([3., 0.5, 2.25])),
            element("m_PlayerID", b"2"),
            element("m_Direction", quarter.to_string().as_bytes()),
        ];
        assert!(doc.encode() == island(&element("Object", &fields.concat()), b""));
    }

    // A copy appended to a list takes a fresh id one past every id the file
    // names, links included; removing a record unlinks its id and drops a
    // group it leaves empty.
    #[test]
    fn copies_take_fresh_ids_and_removals_take_their_links() {
        let groups = element(
            "ObjectGroups",
            &[
                element("g", &blob(&[7u32, 30].map(u32::to_le_bytes).concat())),
                element("g", &blob(&9u32.to_le_bytes())),
            ]
            .concat(),
        );
        let before = island(
            &[handle(7, [0.; 3], 0), handle(9, [1.; 3], 0)].concat(),
            &groups,
        );
        let mut doc = Doc::parse(&before).unwrap();
        let loaded = read(&doc, "isle.isd").unwrap();
        assert_eq!(largest_id(&doc, &loaded), 30);

        let one = Doc::parse(&excerpt(&doc, records(&doc)[0].1).unwrap()).unwrap();
        assert_eq!(read_record(&one, Section::Handle).unwrap(), loaded[0]);
        let copy = append_record(&mut doc, Section::Handle, &one).unwrap();
        renumber(&mut doc, copy, 31).unwrap();

        let mut removed = vec![records(&doc)[1].1];
        unlink(&mut doc, &[9], &mut removed).unwrap();
        doc.remove_all(&removed);
        let after = read(&doc, "isle.isd").unwrap();
        assert_eq!(after.iter().map(|o| o.id).collect::<Vec<_>>(), [7, 31]);
        let groups = element(
            "ObjectGroups",
            &element("g", &blob(&[7u32, 30].map(u32::to_le_bytes).concat())),
        );
        let want = island(
            &[handle(7, [0.; 3], 0), handle(31, [0.; 3], 0)].concat(),
            &groups,
        );
        assert!(doc.encode() == want);

        // A list the file lacks goes in before the lists that follow it.
        add_list(&mut doc, Section::Feedback).unwrap();
        assert!(list(&doc, Section::Feedback).is_some());
        assert_eq!(read(&doc, "isle.isd").unwrap(), after);
    }
}
