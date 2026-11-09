// What makes an island settleable, as read from every shipped pool island:
// harbour spots (a `Coastline` curve on the waterline with a pad of
// construction area reaching ~29 m into the sea), resource slots, a player
// island marker, and an `AreaIDs` grid where construction land meets open
// sea only at those pads. Records are copied from the province's shipped
// islands, since a new island has none to copy from; the same library
// carries the River and Lake records the River tool copies.
use anno_formats::{
    a7minfo::{A7mInfo, RandomSlotGroup, RandomSlotObject, Slots, TypedSlotGroup, TypedSlotObject},
    adf::Adf,
    assets::Assets,
    game::Game,
    grids::Layer,
    objects::{Object, Objects, PathPoint, Template},
    rda::Archive,
    vfs::Install,
};
use anyhow::{Context, Result};
use gpui_editor_kit::heightfield::Heightfield;
use std::collections::{HashMap, VecDeque};
use std::ops::Range;

/// `AreaIDs` keys every shipped pool island uses: the construction area
/// players build in, and the base area (cliffs, rims, nearby sea).
pub const CONSTRUCTION: u32 = 0x2001;
pub const BASE: u32 = 0x0001;
/// Shipped pads reach 27–31 m from their curve into the sea.
const PAD_RADIUS: f32 = 29.;
/// Base-area band between construction land and sea outside the pads, in
/// 1 m cells; shipped islands never let the two touch.
const RIM: u32 = 3;
/// Shipped harbour curves: typically 25–75 m long, points 5–15 m apart.
const COAST_LENGTH: f32 = 48.;
const COAST_STEP: f32 = 10.;
/// Shipped curves sit on the waterline.
const COAST_HEIGHT: [f32; 2] = [0.2, 0.8];
/// Slots stand about 3 m from their work area.
const WORK_AREA_OFFSET: f32 = 3.;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Piece {
    Coast,
    StartCoast,
    Marker,
    /// Random slot: the map generator makes some of them mines.
    MineSlot,
    /// `ActivateAll`: always a mine.
    FixedMineSlot,
    MineWorkArea,
    ClaySlot,
    OilSlot,
    GasSlot,
    NewWorldMineSlot,
    NewWorldMineWorkArea,
    RiverSlot,
    FixedRiverSlot,
    RiverWorkArea,
    /// Celtic marsh drainage slot (5282, template `Slot_Marsh`): no work
    /// area, no fixed variant, no generator slot count.
    MarshSlot,
    /// River and lake EditorObjects (`River` 1001608, `Lake` 1001607), for
    /// their water look components.
    River,
    Lake,
}

impl Piece {
    const ALL: [Piece; 17] = [
        Piece::Coast,
        Piece::StartCoast,
        Piece::Marker,
        Piece::MineSlot,
        Piece::FixedMineSlot,
        Piece::MineWorkArea,
        Piece::ClaySlot,
        Piece::OilSlot,
        Piece::GasSlot,
        Piece::NewWorldMineSlot,
        Piece::NewWorldMineWorkArea,
        Piece::RiverSlot,
        Piece::FixedRiverSlot,
        Piece::RiverWorkArea,
        Piece::MarshSlot,
        Piece::River,
        Piece::Lake,
    ];

    /// Slots and harbours copy only pool islands, where the map generator
    /// uses them. Rivers and lakes are just water, and celtic pool islands
    /// have none, so any island of the province will do.
    fn pool_only(self) -> bool {
        !matches!(self, Piece::River | Piece::Lake)
    }

    /// Which asset plays this part, by template and internal name.
    fn matches(self, template: &str, name: &str) -> bool {
        match self {
            Piece::Coast => template == "Coastline" && name == "Coastline",
            Piece::StartCoast => template == "Coastline" && name.contains("StartCoast"),
            Piece::Marker => template == "PositionMarker" && name.contains("Player Islands"),
            Piece::MineSlot => {
                template == "Slot"
                    && ((name.contains("Mountain") && !name.contains("Fixed"))
                        || name == "random slot mining"
                        || name == "random slot mining arctic")
            }
            Piece::FixedMineSlot => {
                template == "Slot"
                    && ((name.contains("Mountain") && name.contains("Fixed"))
                        || name.contains("(Coal Mine Slot)"))
            }
            Piece::MineWorkArea => {
                template == "WorkAreaSlot"
                    && name.to_ascii_lowercase().contains("mountain")
                    && !name.contains("season4")
            }
            Piece::ClaySlot => template == "Slot" && name == "random slot clay",
            Piece::OilSlot => template == "Slot" && name == "random slot oil pump",
            Piece::GasSlot => template == "Slot" && name == "random slot gas arctic",
            Piece::NewWorldMineSlot => {
                template == "Slot" && name == "season4 random mining slot colony01"
            }
            Piece::NewWorldMineWorkArea => {
                template == "WorkAreaSlot" && name == "season4 slot mining workarea mountain"
            }
            Piece::RiverSlot => {
                template == "Slot"
                    && ((name.contains("River") && !name.contains("Fixed"))
                        || name == "random slot river")
            }
            Piece::FixedRiverSlot => {
                template == "Slot" && name.contains("River") && name.contains("Fixed")
            }
            Piece::RiverWorkArea => {
                (template == "WorkAreaSlot" && name.contains("River"))
                    || template == "WorkAreaRiverSlot"
            }
            Piece::MarshSlot => template == "Slot_Marsh",
            Piece::River => template == "River",
            Piece::Lake => template == "Lake",
        }
    }
}

/// What the slot tool places: a slot and, for mines and rivers, its work
/// area.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Resource {
    Mine,
    Clay,
    Oil,
    Gas,
    NewWorldMine,
    River,
    /// Celtic only: no Roman island ships one, so the Roman library has no
    /// record and the option bar hides it.
    Marsh,
}

impl Resource {
    pub const ALL: [(Resource, &'static str); 7] = [
        (Resource::Mine, "Mine"),
        (Resource::Clay, "Clay"),
        (Resource::Oil, "Oil"),
        (Resource::Gas, "Gas"),
        (Resource::NewWorldMine, "DLC mine"),
        (Resource::River, "River"),
        (Resource::Marsh, "Marsh"),
    ];

    /// Slot and work area pieces; `fixed` slots are always active. Marsh
    /// slots have neither a fixed variant nor a work area.
    pub fn pieces(self, fixed: bool) -> (Piece, Option<Piece>) {
        match (self, fixed) {
            (Resource::Mine, false) => (Piece::MineSlot, Some(Piece::MineWorkArea)),
            (Resource::Mine, true) => (Piece::FixedMineSlot, Some(Piece::MineWorkArea)),
            (Resource::Clay, _) => (Piece::ClaySlot, None),
            (Resource::Oil, _) => (Piece::OilSlot, None),
            (Resource::Gas, _) => (Piece::GasSlot, Some(Piece::MineWorkArea)),
            (Resource::NewWorldMine, _) => {
                (Piece::NewWorldMineSlot, Some(Piece::NewWorldMineWorkArea))
            }
            (Resource::River, false) => (Piece::RiverSlot, Some(Piece::RiverWorkArea)),
            (Resource::River, true) => (Piece::FixedRiverSlot, Some(Piece::RiverWorkArea)),
            (Resource::Marsh, _) => (Piece::MarshSlot, None),
        }
    }

    pub fn has_fixed(self) -> bool {
        matches!(self, Resource::Mine | Resource::River)
    }

    /// `Fertility/NeedsSlot` of the deposits this slot kind serves.
    pub fn slot_type(self) -> &'static str {
        match self {
            Resource::Mine => "Mountain",
            Resource::Clay => "Clay",
            Resource::Oil => "Oil",
            Resource::Gas => "Gas",
            Resource::NewWorldMine => "Mountain",
            Resource::River => "River",
            Resource::Marsh => "Marsh",
        }
    }
}

/// The pieces a region's library needs before it stops reading islands.
/// 117 reads until it has every piece.
fn required_pieces(game: Game, region: &str) -> &'static [Piece] {
    match (game, region) {
        (Game::Anno117, _) => &Piece::ALL,
        (Game::Anno1800, "Moderate") => &[
            Piece::Coast,
            Piece::MineSlot,
            Piece::MineWorkArea,
            Piece::ClaySlot,
            Piece::OilSlot,
            Piece::River,
            Piece::Lake,
        ],
        (Game::Anno1800, "Colony01") => &[
            Piece::Coast,
            Piece::MineSlot,
            Piece::MineWorkArea,
            Piece::ClaySlot,
            Piece::OilSlot,
            Piece::NewWorldMineSlot,
            Piece::NewWorldMineWorkArea,
            Piece::River,
            Piece::Lake,
        ],
        (Game::Anno1800, "Arctic") => &[
            Piece::Coast,
            Piece::MineSlot,
            Piece::MineWorkArea,
            Piece::GasSlot,
        ],
        (Game::Anno1800, "Africa") => &[
            Piece::Coast,
            Piece::RiverSlot,
            Piece::RiverWorkArea,
            Piece::River,
            Piece::Lake,
        ],
        (Game::Anno1800, _) => &[Piece::Coast],
    }
}

/// Template records for each piece, from the province's shipped islands.
pub struct Library {
    templates: HashMap<Piece, Template>,
    slot_kinds: HashMap<u32, u16>,
}

impl Library {
    /// Reads the region's islands, pool islands first, until every piece
    /// has a record. Only `gamedata.data` is decoded, ~50 ms per island.
    pub fn load(install: &Install, assets: &Assets, region: &str) -> Result<Library> {
        let mut paths: Vec<String> = install
            .search(".a7m", 10_000)
            .into_iter()
            .map(|id| install.file_path(id).to_owned())
            .filter(|p| p.ends_with(".a7m") && install.game().island_region(p) == region)
            .collect();
        // Large islands carry the start coast; try them first.
        paths.sort_by_key(|p| {
            (
                !p.contains("/pool/"),
                !(p.contains("large") || p.contains("_l_")),
            )
        });
        let required = required_pieces(install.game(), region);
        let mut templates = HashMap::new();
        let mut slot_kinds = HashMap::new();
        for path in paths {
            if let Ok(info) = install.read(&format!("{path}info"))
                && let Ok(info) = A7mInfo::parse(&info)
                && let Slots::Typed(groups) = info.slots
            {
                for group in groups {
                    for object in group.objects {
                        slot_kinds.insert(object.guid, group.kind);
                    }
                }
            }
            let pool = path.contains("/pool/");
            if !pool
                && Piece::ALL
                    .iter()
                    .all(|p| p.pool_only() || templates.contains_key(p))
            {
                break;
            }
            let read = || -> Result<Objects> {
                let archive = Archive::from_bytes(install.read(&path)?, &path)?;
                let gamedata = Adf::decode(&archive.read_path("gamedata.data")?)?;
                Objects::read(&gamedata).with_context(|| format!("{path}: objects"))
            };
            // One island the parser cannot read (a new DLC's records) must
            // not take the whole tool down; the ensure below still fails
            // loud when nothing usable was found.
            let store = match read() {
                Ok(store) => store,
                Err(e) => {
                    eprintln!("harbour records: skipping {e:#}");
                    continue;
                }
            };
            for (i, o) in store.objects.iter().enumerate() {
                let Some(asset) = assets.get(o.guid) else {
                    continue;
                };
                let template = template_of(assets, o.guid);
                for piece in Piece::ALL {
                    if region != "Colony01"
                        && matches!(piece, Piece::NewWorldMineSlot | Piece::NewWorldMineWorkArea)
                    {
                        continue;
                    }
                    if !templates.contains_key(&piece)
                        && (pool || !piece.pool_only())
                        && piece.matches(&template, &asset.name)
                    {
                        templates.insert(piece, store.template(i));
                    }
                }
            }
            if required.iter().all(|p| templates.contains_key(p)) {
                break;
            }
        }
        anyhow::ensure!(
            templates.contains_key(&Piece::Coast),
            "no shipped {region} island has a coastline to copy"
        );
        // 1800 uses the same coastline asset for regular and starter coasts,
        // which is why its regions do not require a start coast.
        if !required.contains(&Piece::StartCoast)
            && let Some(coast) = templates.get(&Piece::Coast).cloned()
        {
            templates.entry(Piece::StartCoast).or_insert(coast);
        }
        Ok(Library {
            templates,
            slot_kinds,
        })
    }

    pub fn typed_slots(&self, objects: &[Object]) -> Vec<TypedSlotGroup> {
        let mut groups = std::collections::BTreeMap::<u16, Vec<TypedSlotObject>>::new();
        for o in objects {
            if let Some(&kind) = self.slot_kinds.get(&o.guid) {
                groups.entry(kind).or_default().push(TypedSlotObject {
                    guid: o.guid,
                    object: RandomSlotObject {
                        object_id: o.id,
                        position: o.position,
                    },
                });
            }
        }
        groups
            .into_iter()
            .map(|(kind, objects)| TypedSlotGroup { kind, objects })
            .collect()
    }

    pub fn guid(&self, piece: Piece) -> Option<u32> {
        self.templates.get(&piece).map(|t| t.object.guid)
    }

    /// Whether `guid` is one of the slot assets the library places.
    pub fn is_slot(&self, guid: u32) -> bool {
        self.resource_of(guid).is_some()
    }

    /// Which kind of slot `guid` is, random or fixed.
    pub fn resource_of(&self, guid: u32) -> Option<Resource> {
        Resource::ALL.into_iter().map(|(r, _)| r).find(|r| {
            [r.pieces(false).0, r.pieces(true).0]
                .iter()
                .any(|&p| self.guid(p) == Some(guid))
        })
    }

    /// A new object of `piece` at `position`, in the area the grid says.
    pub fn place(
        &self,
        store: &Objects,
        piece: Piece,
        areas: Option<&Layer>,
        position: [f32; 3],
    ) -> Option<Object> {
        let t = self.templates.get(&piece)?;
        let area = areas
            .and_then(|a| {
                a.cell_at(position[0], position[2])
                    .map(|(x, y)| a.get(x, y))
            })
            .filter(|&a| a == CONSTRUCTION || a == BASE)
            .unwrap_or(BASE) as u16;
        Some(store.add_template(t, area, position))
    }
}

/// Shipped records for `guids`, from the first island of any province that
/// has each. For objects whose record carries nothing regional (sound spots,
/// build blockers): large pool islands first, where most of them are, so a
/// handful of islands are read (~50 ms each). Missing GUIDs are left out.
pub fn find_records(install: &Install, guids: &[u32]) -> Result<HashMap<u32, Template>> {
    let mut paths: Vec<String> = install
        .search(".a7m", 10_000)
        .into_iter()
        .map(|id| install.file_path(id).to_owned())
        .filter(|p| p.ends_with(".a7m") && p.contains("/islands/"))
        .collect();
    paths.sort_by_key(|p| (!p.contains("/pool/"), !p.contains("large"), p.clone()));
    let mut found: HashMap<u32, Template> = HashMap::new();
    for path in paths {
        if guids.iter().all(|g| found.contains_key(g)) {
            break;
        }
        let read = || -> Result<Objects> {
            let archive = Archive::from_bytes(install.read(&path)?, &path)?;
            Objects::read(&Adf::decode(&archive.read_path("gamedata.data")?)?)
        };
        let Ok(store) = read() else { continue };
        for (i, o) in store.objects.iter().enumerate() {
            // A record without an editor folder, when there is one: a
            // folder ID means nothing on another island.
            let better = match found.get(&o.guid) {
                None => true,
                Some(t) => t.object.folder.is_some() && o.folder.is_none(),
            };
            if guids.contains(&o.guid) && better {
                found.insert(o.guid, store.template(i));
            }
        }
    }
    Ok(found)
}

/// Template of an asset, following `BaseAssetGUID` for derived ones such as
/// "Fixed Slot Roman Mountain".
/// Which piece an object of `guid` plays, by its asset's template and
/// name: how the island converter recognises slots, coasts and water on an
/// island of the other game.
pub fn piece_of(assets: &Assets, guid: u32) -> Option<Piece> {
    let name = &assets.get(guid)?.name;
    let template = template_of(assets, guid);
    Piece::ALL.into_iter().find(|p| p.matches(&template, name))
}

pub fn template_of(assets: &Assets, guid: u32) -> String {
    let mut at = assets.get(guid);
    for _ in 0..8 {
        let Some(a) = at else { break };
        if !a.template.is_empty() {
            return a.template.clone();
        }
        at = a.base.and_then(|b| assets.get(b));
    }
    String::new()
}

/// `.a7minfo` `RandomSlotObjects`: every slot object (template `Slot`,
/// random or fixed) by asset GUID, ascending as shipped. The map generator
/// picks mines and river spots from this list.
pub fn slot_groups(objects: &[Object], is_slot: impl Fn(u32) -> bool) -> Vec<RandomSlotGroup> {
    let mut groups: std::collections::BTreeMap<u32, Vec<RandomSlotObject>> = Default::default();
    for o in objects {
        if is_slot(o.guid) {
            groups.entry(o.guid).or_default().push(RandomSlotObject {
                object_id: o.id,
                position: o.position,
            });
        }
    }
    groups
        .into_iter()
        .map(|(guid, objects)| RandomSlotGroup { guid, objects })
        .collect()
}

/// `.a7minfo` `StartCoastDirection`: the angle from the land's centre to
/// the start coast, `atan2(z, x)` in 0..2π (within 0.1 rad of the stored
/// value on most shipped start islands).
pub fn start_coast_direction(curve: &[PathPoint], centre: [f32; 2]) -> f32 {
    let mid = curve[curve.len() / 2].p;
    (mid[2] - centre[1])
        .atan2(mid[0] - centre[0])
        .rem_euclid(std::f32::consts::TAU)
}

/// The waterline on the 1 m cell grid: land cells (height above 0) with a
/// 4-neighbour at or below sea level.
struct Shore<'a> {
    field: &'a Heightfield,
    size: [i32; 2],
}

impl<'a> Shore<'a> {
    fn new(field: &'a Heightfield) -> Self {
        let [w, d] = field.grid.size();
        Shore {
            field,
            size: [w as i32, d as i32],
        }
    }

    fn land(&self, x: i32, z: i32) -> bool {
        self.field.height([x as f32 + 0.5, z as f32 + 0.5]) > 0.
    }

    fn inside(&self, x: i32, z: i32) -> bool {
        x >= 0 && z >= 0 && x < self.size[0] && z < self.size[1]
    }

    fn is_shore(&self, x: i32, z: i32) -> bool {
        self.inside(x, z)
            && self.land(x, z)
            && [(1, 0), (-1, 0), (0, 1), (0, -1)]
                .iter()
                .any(|(dx, dz)| !self.land(x + dx, z + dz))
    }

    /// Shore cell nearest `at` within `reach` cells.
    fn nearest(&self, at: [f32; 2], reach: i32) -> Option<(i32, i32)> {
        let (cx, cz) = (at[0] as i32, at[1] as i32);
        (-reach..=reach)
            .flat_map(|dz| (-reach..=reach).map(move |dx| (cx + dx, cz + dz)))
            .filter(|&(x, z)| self.is_shore(x, z))
            .min_by_key(|&(x, z)| (x - cx).pow(2) + (z - cz).pow(2))
    }

    fn neighbours(&self, (x, z): (i32, i32)) -> impl Iterator<Item = (i32, i32)> + '_ {
        (-1..=1)
            .flat_map(move |dz| (-1..=1).map(move |dx| (x + dx, z + dz)))
            .filter(move |&n| n != (x, z) && self.is_shore(n.0, n.1))
    }

    /// Shortest run of shore cells from `a` to `b`, up to `max` cells.
    fn path(&self, a: (i32, i32), b: (i32, i32), max: usize) -> Option<Vec<(i32, i32)>> {
        let mut prev: HashMap<(i32, i32), (i32, i32)> = HashMap::from([(a, a)]);
        let mut queue = VecDeque::from([(a, 0usize)]);
        while let Some((at, n)) = queue.pop_front() {
            if at == b {
                let mut out = vec![b];
                let mut c = b;
                while c != a {
                    c = prev[&c];
                    out.push(c);
                }
                out.reverse();
                return Some(out);
            }
            if n >= max {
                continue;
            }
            for next in self.neighbours(at) {
                if let std::collections::hash_map::Entry::Vacant(e) = prev.entry(next) {
                    e.insert(at);
                    queue.push_back((next, n + 1));
                }
            }
        }
        None
    }

    /// Cells walked from `start` both ways along the shore, `half` metres
    /// each way, preferring to keep direction.
    fn around(&self, start: (i32, i32), half: f32) -> Vec<(i32, i32)> {
        let mut seen = std::collections::HashSet::from([start]);
        let walk = |seen: &mut std::collections::HashSet<(i32, i32)>| {
            let mut out = Vec::new();
            let (mut at, mut dir) = (start, (0, 0));
            let mut length = 0.;
            while length < half {
                let next = self
                    .neighbours(at)
                    .filter(|n| !seen.contains(n))
                    .max_by_key(|&(x, z)| (x - at.0) * dir.0 + (z - at.1) * dir.1);
                let Some(n) = next else { break };
                length += (((n.0 - at.0).pow(2) + (n.1 - at.1).pow(2)) as f32).sqrt();
                dir = (n.0 - at.0, n.1 - at.1);
                seen.insert(n);
                out.push(n);
                at = n;
            }
            out
        };
        let ahead = walk(&mut seen);
        let behind = walk(&mut seen);
        behind
            .into_iter()
            .rev()
            .chain(std::iter::once(start))
            .chain(ahead)
            .collect()
    }
}

/// A harbour curve along the waterline through the shore nearest `at`, or
/// None when there is no shore within reach.
pub fn trace_coast(field: &Heightfield, at: [f32; 2]) -> Option<Vec<PathPoint>> {
    let shore = Shore::new(field);
    let start = shore.nearest(at, 24)?;
    curve_from_cells(field, &shore.around(start, COAST_LENGTH / 2.))
}

/// A harbour curve following the shore from near `a` to near `b`.
pub fn trace_between(field: &Heightfield, a: [f32; 2], b: [f32; 2]) -> Option<Vec<PathPoint>> {
    // 300 m of shore is far longer than any shipped harbour (≤ 120 m).
    const MAX_CELLS: usize = 300;
    let shore = Shore::new(field);
    let (from, to) = (shore.nearest(a, 24)?, shore.nearest(b, 24)?);
    curve_from_cells(field, &shore.path(from, to, MAX_CELLS)?)
}

/// `at` moved onto the nearest waterline cell within 12 m.
pub fn snap_to_shore(field: &Heightfield, at: [f32; 2]) -> Option<[f32; 2]> {
    let (x, z) = Shore::new(field).nearest(at, 12)?;
    Some([x as f32 + 0.5, z as f32 + 0.5])
}

fn curve_from_cells(field: &Heightfield, cells: &[(i32, i32)]) -> Option<Vec<PathPoint>> {
    let cells: Vec<[f32; 2]> = cells
        .iter()
        .map(|&(x, z)| [x as f32 + 0.5, z as f32 + 0.5])
        .collect();
    if cells.len() < 8 {
        return None;
    }
    // Smooth the staircase, then resample at even spacing.
    let smooth: Vec<[f32; 2]> = (0..cells.len())
        .map(|i| {
            let r = &cells[i.saturating_sub(3)..(i + 4).min(cells.len())];
            let n = r.len() as f32;
            [
                r.iter().map(|c| c[0]).sum::<f32>() / n,
                r.iter().map(|c| c[1]).sum::<f32>() / n,
            ]
        })
        .collect();
    let mut points = vec![smooth[0]];
    let mut carried = 0.;
    for w in smooth.windows(2) {
        carried += (w[1][0] - w[0][0]).hypot(w[1][1] - w[0][1]);
        if carried >= COAST_STEP {
            points.push(w[1]);
            carried = 0.;
        }
    }
    if carried > COAST_STEP / 2. {
        points.push(*smooth.last().unwrap());
    }
    (points.len() >= 3).then(|| with_directions(field, &points))
}

/// Curve points through `points`: waterline height, and `d` toward the sea.
pub fn with_directions(field: &Heightfield, points: &[[f32; 2]]) -> Vec<PathPoint> {
    let sea = |x: f32, z: f32| field.height([x, z]) <= 0.;
    (0..points.len())
        .map(|i| {
            let (a, b) = (
                points[i.saturating_sub(1)],
                points[(i + 1).min(points.len() - 1)],
            );
            let (tx, tz) = (b[0] - a[0], b[1] - a[1]);
            let l = tx.hypot(tz).max(1e-3);
            let (mut nx, mut nz) = (tz / l, -tx / l);
            let [x, z] = points[i];
            if !sea(x + nx * 6., z + nz * 6.) {
                (nx, nz) = (-nx, -nz);
            }
            PathPoint {
                p: [
                    x,
                    field.height([x, z]).clamp(COAST_HEIGHT[0], COAST_HEIGHT[1]),
                    z,
                ],
                d: Some(snap_direction(nx, nz)),
                ..Default::default()
            }
        })
        .collect()
}

/// `d` of shipped curve points: the seaward direction snapped to 45°.
fn snap_direction(x: f32, z: f32) -> [f32; 2] {
    let step = std::f32::consts::FRAC_PI_4;
    let a = (z.atan2(x) / step).round() * step;
    let r = |v: f32| if v.abs() < 1e-6 { 0. } else { v };
    [r(a.cos()), r(a.sin())]
}

/// An axis-aligned region of the island, in metres.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Region {
    pub min: [f32; 2],
    pub max: [f32; 2],
}

impl Region {
    pub fn spanning(a: [f32; 2], b: [f32; 2]) -> Region {
        Region {
            min: [a[0].min(b[0]), a[1].min(b[1])],
            max: [a[0].max(b[0]), a[1].max(b[1])],
        }
    }

    fn contains(&self, p: [f32; 2]) -> bool {
        (self.min[0]..=self.max[0]).contains(&p[0]) && (self.min[1]..=self.max[1]).contains(&p[1])
    }

    /// Cell centres every `step` metres.
    fn samples(&self, step: f32) -> impl Iterator<Item = [f32; 2]> + '_ {
        let nx = ((self.max[0] - self.min[0]) / step) as usize;
        let nz = ((self.max[1] - self.min[1]) / step) as usize;
        (0..=nz).flat_map(move |j| {
            (0..=nx).map(move |i| [self.min[0] + i as f32 * step, self.min[1] + j as f32 * step])
        })
    }
}

/// Harbours must stand at least this far apart, centre to centre.
const HARBOUR_GAP: f32 = 70.;
/// Ships need water at least this deep 25 m out from a harbour.
const HARBOUR_DEPTH: f32 = 2.;

/// Up to `count` harbour curves on the shore inside `region`, best first:
/// flat ground behind, deep water in front, apart from each other and from
/// `existing` curves.
pub fn auto_harbours(
    field: &Heightfield,
    region: Region,
    existing: &[&[PathPoint]],
    count: usize,
) -> Vec<Vec<PathPoint>> {
    let shore = Shore::new(field);
    let h = |x: f32, z: f32| field.height([x, z]);
    let mut candidates: Vec<([f32; 2], f32)> = region
        .samples(3.)
        .filter_map(|at| {
            let (x, z) = shore.nearest(at, 1)?;
            let c = [x as f32 + 0.5, z as f32 + 0.5];
            // Seaward: the mean direction to sea cells around.
            let (mut sx, mut sz) = (0., 0.);
            for k in 0..16 {
                let a = k as f32 / 16. * std::f32::consts::TAU;
                if h(c[0] + a.cos() * 8., c[1] + a.sin() * 8.) <= 0. {
                    sx += a.cos();
                    sz += a.sin();
                }
            }
            let l = f32::hypot(sx, sz);
            if l < 1e-3 {
                return None;
            }
            let (sx, sz) = (sx / l, sz / l);
            let depth = -h(c[0] + sx * 25., c[1] + sz * 25.);
            if depth < HARBOUR_DEPTH {
                return None;
            }
            // Lower and flatter ground behind reads as a beach.
            let behind = h(c[0] - sx * 12., c[1] - sz * 12.).max(0.)
                + h(c[0] - sx * 24., c[1] - sz * 24.).max(0.) * 0.5;
            Some((c, behind))
        })
        .collect();
    candidates.sort_by(|a, b| a.1.total_cmp(&b.1));
    // Curves keep their distance end to end, not only centre to centre.
    let clear = |a: &[PathPoint], b: &[PathPoint]| {
        a.iter().all(|p| {
            b.iter()
                .all(|q| (p.p[0] - q.p[0]).hypot(p.p[2] - q.p[2]) >= HARBOUR_GAP / 2.)
        })
    };
    let mut out: Vec<Vec<PathPoint>> = Vec::new();
    for (c, _) in candidates {
        if out.len() >= count {
            break;
        }
        let Some(curve) = trace_coast(field, c) else {
            continue;
        };
        let mid = curve[curve.len() / 2].p;
        if region.contains([mid[0], mid[2]])
            && existing.iter().all(|e| clear(&curve, e))
            && out.iter().all(|e| clear(&curve, e))
        {
            out.push(curve);
        }
    }
    out
}

/// Slots stand at least this far apart.
const SLOT_GAP: f32 = 30.;
/// Shipped marsh slots (94 on 20 Celtic islands, `marsh_slots` example):
/// 0.2–0.7 m above sea level, 2–57 m (median 8, 90% within 21) from open
/// sea, never near rivers, 9 m apart at the closest tenth (median 31).
const MARSH_HEIGHT: [f32; 2] = [0.2, 0.8];
const MARSH_SEA_REACH: f32 = 24.;
const MARSH_GAP: f32 = 20.;
/// `EnvironmentGRid` value of the Marsh ambience; under 46% of shipped
/// marsh slots (most of the rest sit on the default type 1).
const MARSH_ENVIRONMENT: u32 = 7;

/// Up to `count` slot positions inside `region`: mines on high, steep
/// ground; river slots on land next to river cells; marsh slots on flat
/// ground just above sea level near the coast, Marsh ambience first. Keeps
/// clear of `existing` slots and of each other.
pub fn auto_slots(
    field: &Heightfield,
    river: Option<&Layer>,
    environment: Option<&Layer>,
    region: Region,
    resource: Resource,
    existing: &[[f32; 2]],
    count: usize,
) -> Vec<[f32; 3]> {
    let h = |x: f32, z: f32| field.height([x, z]);
    let cells_near = |layer: Option<&Layer>, x: f32, z: f32, hit: &dyn Fn(u32) -> bool| {
        let Some(r) = layer else { return 0 };
        (-4..=4)
            .flat_map(|dz| (-4..=4).map(move |dx| (dx, dz)))
            .filter(|&(dx, dz)| {
                r.cell_at(x + dx as f32, z + dz as f32)
                    .is_some_and(|(i, j)| hit(r.get(i, j)))
            })
            .count()
    };
    // Metres to the nearest sea sample on 16 rays, up to `reach`.
    let sea_within = |x: f32, z: f32, reach: f32| {
        (1..=(reach / 2.) as usize)
            .map(|k| k as f32 * 2.)
            .find(|&r| {
                (0..16).any(|k| {
                    let a = k as f32 / 16. * std::f32::consts::TAU;
                    h(x + a.cos() * r, z + a.sin() * r) <= 0.
                })
            })
    };
    let mut candidates: Vec<([f32; 3], f32)> = region
        .samples(4.)
        .filter_map(|[x, z]| {
            let y = h(x, z);
            let slope = (h(x + 2., z) - h(x - 2., z)).hypot(h(x, z + 2.) - h(x, z - 2.)) / 4.;
            let score = match resource {
                Resource::Marsh => {
                    if !(MARSH_HEIGHT[0]..=MARSH_HEIGHT[1]).contains(&y)
                        || cells_near(river, x, z, &|v| v != 0) > 0
                    {
                        return None;
                    }
                    let sea = sea_within(x, z, MARSH_SEA_REACH)?;
                    // Shipped slots sit at a median 8 m from the sea.
                    let marshy = cells_near(environment, x, z, &|v| v == MARSH_ENVIRONMENT);
                    marshy as f32 * 0.1 - (sea - 8.).abs() * 0.1 - slope * 20.
                }
                _ => {
                    // Mines and river slots sit inland: near the waterline
                    // the drop into the sea would read as a steep mountain.
                    if y <= 1. || sea_within(x, z, 8.).is_some() {
                        return None;
                    }
                    match resource {
                        Resource::Mine | Resource::NewWorldMine => y + slope * 8.,
                        Resource::Clay | Resource::Oil | Resource::Gas => {
                            -slope * 20. + y.min(20.) * 0.1
                        }
                        Resource::River | Resource::Marsh => {
                            match cells_near(river, x, z, &|v| v != 0) {
                                0 => return None,
                                n => n as f32 - slope * 4.,
                            }
                        }
                    }
                }
            };
            Some(([x, y, z], score))
        })
        .collect();
    candidates.sort_by(|a, b| b.1.total_cmp(&a.1));
    let gap = if resource == Resource::Marsh {
        MARSH_GAP
    } else {
        SLOT_GAP
    };
    let mut out: Vec<[f32; 3]> = Vec::new();
    for (p, _) in candidates {
        if out.len() >= count {
            break;
        }
        let clear = |q: [f32; 2]| (q[0] - p[0]).hypot(q[1] - p[2]) >= gap;
        if existing.iter().all(|&q| clear(q)) && out.iter().all(|q| clear([q[0], q[2]])) {
            out.push(p);
        }
    }
    out
}

/// Shipped construction areas stop at about 15°: 98% of their land is
/// gentler, and none is steeper than 45°.
const MAX_BUILD_SLOPE: f32 = 15.;
/// Buildable patches smaller than this many m² are dropped, and enclosed
/// unbuildable specks smaller than `MIN_HOLE` filled, so slope noise does
/// not speckle the area like no shipped island is.
pub const MIN_PATCH: usize = 150;
const MIN_HOLE: usize = 40;

/// What land away from the coast becomes when areas are derived.
pub enum Interior<'a> {
    /// Buildable up to `MAX_BUILD_SLOPE`, from the terrain.
    Slope(&'a Heightfield),
    /// What the grid already says (hand-painted), land without an area
    /// becoming buildable.
    Keep,
}

/// Rewrites `areas` from `ground` (the `Water` grid: island ground, 0 is
/// open sea) and the harbour curves. Sea within the pad radius of a curve
/// joins the construction area; land near sea outside the pads gets a
/// base-area rim; other land follows `interior`. Other sea leaves the
/// island. With `window` (world metres `[x0, z0, x1, z1]`) only the cells
/// in it: patch cleanup sees the window's edge as the map's.
pub fn derive_areas(
    ground: &Layer,
    areas: &mut Layer,
    coasts: &[&[PathPoint]],
    interior: Interior,
    window: Option<[f32; 4]>,
) {
    let (w, h) = (areas.width, areas.height);
    let cells = areas.cells_within(window);
    let is_ground = |x: usize, y: usize| {
        let (gx, gy) = (x * ground.width / w, y * ground.height / h);
        ground.get(gx.min(ground.width - 1), gy.min(ground.height - 1)) != 0
    };
    let inner: Vec<u32> = match interior {
        Interior::Slope(field) => buildable_from_slope(field, w, h, &cells),
        Interior::Keep => (0..w * h)
            .map(|k| match areas.get(k % w, k / w) {
                BASE => BASE,
                _ => CONSTRUCTION,
            })
            .collect(),
    };
    let values = area_values(is_ground, w, h, coasts, &inner, &cells);
    for y in cells.1 {
        for x in cells.0.clone() {
            areas.set(x, y, values[y * w + x]);
        }
    }
}

/// Columns and rows of a derivation's window.
type Cells = (Range<usize>, Range<usize>);

/// Cells outside a derivation's window; never flipped or seeded.
const OUTSIDE: u32 = u32::MAX;

/// Construction or base per 1 m cell from the terrain slope, with small
/// patches and holes cleaned up, in `cells` (`OUTSIDE` elsewhere).
fn buildable_from_slope(field: &Heightfield, w: usize, h: usize, cells: &Cells) -> Vec<u32> {
    let limit = MAX_BUILD_SLOPE.to_radians().tan();
    let height = |x: f32, z: f32| field.height([x, z]);
    let mut out = vec![OUTSIDE; w * h];
    for y in cells.1.clone() {
        for x in cells.0.clone() {
            let (fx, fz) = (x as f32 + 0.5, y as f32 + 0.5);
            let dx = (height(fx + 1., fz) - height(fx - 1., fz)) / 2.;
            let dz = (height(fx, fz + 1.) - height(fx, fz - 1.)) / 2.;
            out[y * w + x] = if dx.hypot(dz) <= limit {
                CONSTRUCTION
            } else {
                BASE
            };
        }
    }
    // Flip components of `value` smaller than `min` to the other value.
    let mut clean = |value: u32, min: usize, other: u32| {
        let mut seen = vec![false; w * h];
        for start in cells
            .1
            .clone()
            .flat_map(|y| cells.0.clone().map(move |x| y * w + x))
        {
            if seen[start] || out[start] != value {
                continue;
            }
            let mut component = vec![start];
            seen[start] = true;
            let mut i = 0;
            while i < component.len() {
                let k = component[i];
                let (x, y) = (k % w, k / w);
                let next = [
                    (x > 0).then(|| k - 1),
                    (x + 1 < w).then(|| k + 1),
                    (y > 0).then(|| k - w),
                    (y + 1 < h).then(|| k + w),
                ];
                for n in next.into_iter().flatten() {
                    if !seen[n] && out[n] == value {
                        seen[n] = true;
                        component.push(n);
                    }
                }
                i += 1;
            }
            if component.len() < min {
                for k in component {
                    out[k] = other;
                }
            }
        }
    };
    clean(CONSTRUCTION, MIN_PATCH, BASE);
    clean(BASE, MIN_HOLE, CONSTRUCTION);
    out
}

/// [`derive_areas`] on plain cells: 1 m cells, `is_ground(x, y)`, and the
/// interior value of each land cell; only `cells` are meaningful.
fn area_values(
    is_ground: impl Fn(usize, usize) -> bool,
    w: usize,
    h: usize,
    coasts: &[&[PathPoint]],
    inner: &[u32],
    cells: &Cells,
) -> Vec<u32> {
    // Each segment marks the sea cells within the pad radius of it; testing
    // every cell against every segment took most of a stroke's release.
    let mut pad = vec![false; w * h];
    for s in coasts.iter().flat_map(|c| c.windows(2)) {
        let (a, b) = (s[0].p, s[1].p);
        let span = |lo: f32, hi: f32, within: &Range<usize>| {
            let first = ((lo - PAD_RADIUS - 0.5).floor().max(0.) as usize).max(within.start);
            let end = ((hi + PAD_RADIUS - 0.5).ceil().max(0.) as usize + 1).min(within.end);
            first.min(end)..end
        };
        let xs = span(a[0].min(b[0]), a[0].max(b[0]), &cells.0);
        for y in span(a[2].min(b[2]), a[2].max(b[2]), &cells.1) {
            for x in xs.clone() {
                let k = y * w + x;
                if !pad[k]
                    && !is_ground(x, y)
                    && segment_distance([x as f32 + 0.5, y as f32 + 0.5], a, b) <= PAD_RADIUS
                {
                    pad[k] = true;
                }
            }
        }
    }
    // Distance in cells from sea that is not a pad.
    let mut dist = vec![u32::MAX; w * h];
    let mut queue = VecDeque::new();
    for y in cells.1.clone() {
        for x in cells.0.clone() {
            if !is_ground(x, y) && !pad[y * w + x] {
                dist[y * w + x] = 0;
                queue.push_back((x, y));
            }
        }
    }
    while let Some((x, y)) = queue.pop_front() {
        let next = dist[y * w + x] + 1;
        if next > RIM {
            continue;
        }
        for (dx, dy) in [(1i32, 0i32), (-1, 0), (0, 1), (0, -1)] {
            let (nx, ny) = (x as i32 + dx, y as i32 + dy);
            if nx < 0 || ny < 0 || nx >= w as i32 || ny >= h as i32 {
                continue;
            }
            let k = ny as usize * w + nx as usize;
            if dist[k] > next {
                dist[k] = next;
                queue.push_back((nx as usize, ny as usize));
            }
        }
    }
    let mut out = vec![OUTSIDE; w * h];
    for y in cells.1.clone() {
        for x in cells.0.clone() {
            let k = y * w + x;
            out[k] = if pad[k] {
                CONSTRUCTION
            } else if !is_ground(x, y) {
                0
            } else if dist[k] <= RIM {
                BASE
            } else {
                inner[k]
            };
        }
    }
    out
}

fn segment_distance(p: [f32; 2], a: [f32; 3], b: [f32; 3]) -> f32 {
    let (ax, az, bx, bz) = (a[0], a[2], b[0], b[2]);
    let (dx, dz) = (bx - ax, bz - az);
    let l2 = dx * dx + dz * dz;
    let t = if l2 == 0. {
        0.
    } else {
        (((p[0] - ax) * dx + (p[1] - az) * dz) / l2).clamp(0., 1.)
    };
    (p[0] - ax - t * dx).hypot(p[1] - az - t * dz)
}

/// Points a placed work area at its slot. Shipped work areas carry a
/// two-point path from themselves to the slot they serve (2–4.5 m), both
/// points with tangents of a sixth of the span, and face roughly along it;
/// a template copied from another island would still point at that
/// island's slot.
pub fn link_work_area(work: &mut Object, slot: [f32; 3]) {
    let at = work.position;
    let span: [f32; 3] = std::array::from_fn(|k| (at[k] - slot[k]) / 6.);
    let point = |p| PathPoint {
        p,
        i: Some(span),
        o: Some(span.map(|v| -v)),
        ..Default::default()
    };
    work.path = Some(vec![point(at), point(slot)]);
    work.set_direction(
        (slot[2] - at[2])
            .atan2(slot[0] - at[0])
            .rem_euclid(std::f32::consts::TAU),
    );
}

/// Where a slot's work area goes: a few metres downhill from it.
pub fn work_area_position(field: &Heightfield, at: [f32; 3]) -> [f32; 3] {
    let h = |x: f32, z: f32| field.height([x, z]);
    let (gx, gz) = (
        h(at[0] + 1., at[2]) - h(at[0] - 1., at[2]),
        h(at[0], at[2] + 1.) - h(at[0], at[2] - 1.),
    );
    let l = gx.hypot(gz);
    let (dx, dz) = if l < 1e-3 {
        (1., 0.)
    } else {
        (-gx / l, -gz / l)
    };
    let (x, z) = (at[0] + dx * WORK_AREA_OFFSET, at[2] + dz * WORK_AREA_OFFSET);
    [x, h(x, z), z]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::island::test_support::{assets, install, install_1800};
    use gpui_editor_kit::heightfield::Grid;

    /// A round island of radius 40 m in a 128 m square, 0.5 m samples.
    fn round_island() -> Heightfield {
        let grid = Grid {
            width: 257,
            depth: 257,
            spacing: 0.5,
            origin: [0.; 2],
        };
        let mut heights = vec![0.; grid.width * grid.depth];
        for z in 0..grid.depth {
            for x in 0..grid.width {
                let [wx, wz] = grid.world(x, z);
                let r = (wx - 64.).hypot(wz - 64.);
                heights[z * grid.width + x] = if r < 40. { 3. } else { -6. };
            }
        }
        Heightfield::new(grid, heights)
    }

    /// A round island like `round_island` with a hill in the middle and
    /// deep water around.
    fn hilly_island() -> Heightfield {
        let mut field = round_island();
        let grid = field.grid;
        for z in 0..grid.depth {
            for x in 0..grid.width {
                let [wx, wz] = grid.world(x, z);
                let r = (wx - 64.).hypot(wz - 64.);
                if r < 40. {
                    field.heights[z * grid.width + x] = 3. + (24. - r * 0.6).max(0.);
                }
            }
        }
        field
    }

    // The coast traced from a click follows the waterline and faces the
    // sea, and its pad is the only place construction land meets open sea.
    #[test]
    fn coast_follows_the_waterline_and_faces_the_sea() {
        let field = round_island();
        let curve = trace_coast(&field, [64., 20.]).expect("shore near the top");
        assert!(curve.len() >= 4, "{} points", curve.len());
        let length: f32 = curve
            .windows(2)
            .map(|w| (w[1].p[0] - w[0].p[0]).hypot(w[1].p[2] - w[0].p[2]))
            .sum();
        assert!((30. ..60.).contains(&length), "length {length}");
        for pt in &curve {
            let r = (pt.p[0] - 64.).hypot(pt.p[2] - 64.);
            assert!((37. ..42.).contains(&r), "on the shore: r = {r}");
            // Seaward is away from the centre.
            let out = [(pt.p[0] - 64.) / r, (pt.p[2] - 64.) / r];
            let d = pt.d.expect("coast direction");
            assert!(
                d[0] * out[0] + d[1] * out[1] > 0.6,
                "d {:?} at {:?}",
                pt.d,
                pt.p
            );
            assert!((COAST_HEIGHT[0]..=COAST_HEIGHT[1]).contains(&pt.p[1]));
        }
        assert!(
            trace_coast(&field, [64., 64.]).is_none(),
            "no shore near the centre"
        );

        let ground = |x: usize, y: usize| field.height([x as f32 + 0.5, y as f32 + 0.5]) > 0.;
        let areas = area_values(
            ground,
            128,
            128,
            &[&curve],
            &vec![CONSTRUCTION; 128 * 128],
            &(0..128, 0..128),
        );
        let at = |x: usize, y: usize| areas[y * 128 + x];
        let (mut pad, mut touching) = (0, 0);
        for y in 1..127 {
            for x in 1..127 {
                let a = at(x, y);
                if !ground(x, y) && a == CONSTRUCTION {
                    pad += 1;
                }
                if ground(x, y) {
                    assert_ne!(a, 0, "land outside every area at {x},{y}");
                }
                if ground(x, y) && a == CONSTRUCTION {
                    for (nx, ny) in [(x + 1, y), (x - 1, y), (x, y + 1), (x, y - 1)] {
                        if !ground(nx, ny) && at(nx, ny) != CONSTRUCTION {
                            touching += 1;
                        }
                    }
                }
            }
        }
        assert!(pad > 500, "the pad reaches into the sea: {pad} cells");
        assert_eq!(touching, 0);
        assert_eq!(at(64, 64), CONSTRUCTION, "the interior is buildable");
    }

    #[test]
    fn dragging_along_the_shore_follows_it() {
        let field = round_island();
        // Quarter turn from the top of the island to its right side.
        let curve = trace_between(&field, [64., 22.], [106., 64.]).unwrap();
        let (first, last) = (curve[0].p, curve[curve.len() - 1].p);
        assert!(
            (first[0] - 64.).abs() < 6. && (first[2] - 24.).abs() < 6.,
            "{first:?}"
        );
        assert!(
            (last[0] - 104.).abs() < 6. && (last[2] - 64.).abs() < 6.,
            "{last:?}"
        );
        for pt in &curve {
            let r = (pt.p[0] - 64.).hypot(pt.p[2] - 64.);
            assert!((36. ..43.).contains(&r), "on the shore: r = {r}");
        }
        let snapped = snap_to_shore(&field, [64., 30.]).unwrap();
        assert!(((snapped[0] - 64.).hypot(snapped[1] - 64.) - 39.5).abs() < 1.5);
    }

    #[test]
    fn auto_harbours_stay_apart_inside_the_region() {
        let field = round_island();
        let all = Region::spanning([0., 0.], [128., 128.]);
        let curves = auto_harbours(&field, all, &[], 10);
        assert!(
            curves.len() >= 2 && curves.len() < 10,
            "{} harbours",
            curves.len()
        );
        let mid = |c: &[PathPoint]| c[c.len() / 2].p;
        let gap = |a: &[PathPoint], b: &[PathPoint]| {
            a.iter()
                .flat_map(|p| {
                    b.iter()
                        .map(move |q| (p.p[0] - q.p[0]).hypot(p.p[2] - q.p[2]))
                })
                .fold(f32::MAX, f32::min)
        };
        for (i, a) in curves.iter().enumerate() {
            for b in &curves[i + 1..] {
                assert!(
                    gap(a, b) >= HARBOUR_GAP / 2.,
                    "curves {} m apart",
                    gap(a, b)
                );
            }
        }
        // Only the top half: every harbour there, none below.
        let top = Region::spanning([0., 0.], [128., 50.]);
        let some = auto_harbours(&field, top, &[], 4);
        assert!(!some.is_empty());
        assert!(some.iter().all(|c| mid(c)[2] <= 50.));
        // Existing harbours are kept clear of.
        let again = auto_harbours(&field, top, &[&some[0]], 4);
        assert!(again.iter().all(|c| {
            (mid(c)[0] - mid(&some[0])[0]).hypot(mid(c)[2] - mid(&some[0])[2]) >= HARBOUR_GAP / 2.
        }));
    }

    #[test]
    fn auto_mines_take_the_high_ground() {
        let field = hilly_island();
        let all = Region::spanning([0., 0.], [128., 128.]);
        let mines = auto_slots(&field, None, None, all, Resource::Mine, &[], 3);
        assert_eq!(mines.len(), 3);
        for m in &mines {
            assert!(m[1] > 5., "mine at height {}", m[1]);
        }
        for (i, a) in mines.iter().enumerate() {
            for b in &mines[i + 1..] {
                assert!((a[0] - b[0]).hypot(a[2] - b[2]) >= SLOT_GAP);
            }
        }
        assert!(
            auto_slots(&field, None, None, all, Resource::River, &[], 3).is_empty(),
            "no rivers, no river slots"
        );
    }

    #[test]
    fn auto_marsh_slots_take_low_ground_by_the_sea() {
        // A round island whose rim drops to a 0.5 m flat 30–40 m from the
        // centre: marsh slots belong on that flat, not on the hill.
        let mut field = hilly_island();
        let grid = field.grid;
        for z in 0..grid.depth {
            for x in 0..grid.width {
                let [wx, wz] = grid.world(x, z);
                let r = (wx - 64.).hypot(wz - 64.);
                if (30. ..40.).contains(&r) {
                    field.heights[z * grid.width + x] = 0.5;
                }
            }
        }
        let all = Region::spanning([0., 0.], [128., 128.]);
        let marsh = auto_slots(&field, None, None, all, Resource::Marsh, &[], 4);
        assert_eq!(marsh.len(), 4);
        for m in &marsh {
            let r = (m[0] - 64.).hypot(m[2] - 64.);
            assert!((30. ..40.).contains(&r), "on the flat: r = {r}");
            assert!((MARSH_HEIGHT[0]..=MARSH_HEIGHT[1]).contains(&m[1]));
        }
        for (i, a) in marsh.iter().enumerate() {
            for b in &marsh[i + 1..] {
                assert!((a[0] - b[0]).hypot(a[2] - b[2]) >= MARSH_GAP);
            }
        }
        assert!(
            auto_slots(&hilly_island(), None, None, all, Resource::Marsh, &[], 4).is_empty(),
            "a 3 m cliff above the sea is no marsh"
        );
    }

    #[test]
    fn steep_ground_is_not_buildable() {
        // A cone: gentle top, 30° flanks.
        let grid = Grid {
            width: 257,
            depth: 257,
            spacing: 0.5,
            origin: [0.; 2],
        };
        let mut heights = vec![0.; grid.width * grid.depth];
        for z in 0..grid.depth {
            for x in 0..grid.width {
                let [wx, wz] = grid.world(x, z);
                let r = (wx - 64.).hypot(wz - 64.);
                heights[z * grid.width + x] = if r < 20. {
                    20.
                } else {
                    (20. - (r - 20.) * 0.58).max(-6.)
                };
            }
        }
        let field = Heightfield::new(grid, heights);
        let inner = buildable_from_slope(&field, 128, 128, &(0..128, 0..128));
        assert_eq!(inner[64 * 128 + 64], CONSTRUCTION, "flat top");
        assert_eq!(inner[64 * 128 + 64 + 30], BASE, "30° flank");
        // Hand-painted interiors survive a keep-derive.
        let ground = |x: usize, y: usize| field.height([x as f32 + 0.5, y as f32 + 0.5]) > 0.;
        let mut painted = vec![CONSTRUCTION; 128 * 128];
        painted[64 * 128 + 64] = BASE;
        let kept = area_values(ground, 128, 128, &[], &painted, &(0..128, 0..128));
        assert_eq!(kept[64 * 128 + 64], BASE);
    }

    #[test]
    fn library_finds_every_piece_on_shipped_islands() {
        let Some(install) = install() else {
            return;
        };
        let assets = assets(&install);
        for region in ["Roman", "Celtic"] {
            let lib = Library::load(&install, &assets, region).unwrap();
            let missing: Vec<_> = Piece::ALL
                .iter()
                .filter(|p| lib.guid(**p).is_none())
                .collect();
            eprintln!("{region}: missing {missing:?}");
            assert!(lib.guid(Piece::Coast).is_some() && lib.guid(Piece::MineSlot).is_some());
            // Marsh slots ship on Celtic islands only.
            assert_eq!(
                lib.guid(Piece::MarshSlot),
                (region == "Celtic").then_some(5282),
                "{region}"
            );
        }
    }

    #[test]
    fn installed_1800_resource_templates_cover_all_regions() {
        let Some(install) = install_1800() else {
            return;
        };
        let assets = assets(&install);
        for (region, guids) in [
            ("Moderate", &[1000029, 100417, 100849][..]),
            ("Colony01", &[1000029, 100417, 100849, 614][..]),
            ("Arctic", &[116037, 113750][..]),
            ("Africa", &[117741][..]),
        ] {
            let lib = Library::load(&install, &assets, region).unwrap();
            for &guid in guids {
                assert!(
                    lib.resource_of(guid).is_some(),
                    "{region}: no tool for {guid}"
                );
            }
            if region == "Africa" {
                assert_eq!(lib.guid(Piece::RiverWorkArea), Some(117742));
            }
        }
    }
}
