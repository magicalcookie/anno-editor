// Harbour pieces and work-area roads: the parts of a settleable island the
// editor copies from shipped islands, the library of shipped records each is
// copied from, and how a game lays the path from a work area to its slot.
// Each game crate says which pieces its regions need and the shape of its
// roads (`Module`).
use crate::game::{Harbours, Module};
use crate::island::objects::{Object, Objects, PathPoint, Template};
use anno_formats::{
    a7m::{
        a7minfo::{A7mInfo, RandomSlotObject, Slots, TypedSlotGroup, TypedSlotObject},
        grids::Layer,
    },
    adf::Adf,
    assets::Assets,
    rda::Archive,
    vfs::Install,
};
use anyhow::{Context as _, Result};
use gpui_editor_kit::heightfield::Heightfield;
use std::collections::HashMap;

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
    /// 1800 Old World mines' second work area, by the slot.
    CaveWorkArea,
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
    pub const ALL: [Piece; 18] = [
        Piece::Coast,
        Piece::StartCoast,
        Piece::Marker,
        Piece::MineSlot,
        Piece::FixedMineSlot,
        Piece::MineWorkArea,
        Piece::CaveWorkArea,
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

    /// Pieces only some of a game's regions have (the New World's mines):
    /// the harbour library takes them only for a region that asks for them
    /// (`Harbours::required_pieces`).
    pub fn regional(self) -> bool {
        matches!(self, Piece::NewWorldMineSlot | Piece::NewWorldMineWorkArea)
    }

    /// Slots and harbours copy only pool islands, where the map generator
    /// uses them. Rivers and lakes are just water, and celtic pool islands
    /// have none, so any island of the province will do.
    pub fn pool_only(self) -> bool {
        !matches!(self, Piece::River | Piece::Lake)
    }
}

/// How a game lays one kind of work area and the path from it to its slot,
/// measured on every shipped island. In 1800 the path is the mine's rail:
/// a trestle ramp from the work area down to the slot. 1800 Old World
/// mines carry two work areas, a mountain one up the hill and a cave one
/// by the slot; one slot can serve both. Angles are yaws as
/// `Object::direction` counts them: `atan2(z, x)`, so 0 is +x.
#[derive(Clone, Copy, Debug)]
pub struct RoadShape {
    /// Ground metres from the slot to its work area.
    pub span: f32,
    /// Where the work area stands around its slot: radians from the
    /// slot's own yaw. Shipped work areas of every kind keep to their
    /// slot's frame far tighter than to the slope around it.
    pub side: f32,
    /// The work area's yaw, radians from the direction to its slot. Which
    /// way a model's opening points is the model's own business: 1800's
    /// mountain work area opens along +x, its cave one along -x.
    pub facing: f32,
    /// The model's path dummy (`StartPositionDummyName`) in its own
    /// frame, x along its yaw: the game starts the path there
    /// (`TransformOnlyStartPoint`), and shipped first points sit exactly
    /// on it. Zero starts the path on the work area itself.
    pub start: [f32; 3],
    /// Ground metres between path points; 0 for a two-point path.
    pub spacing: f32,
    /// Inner points ride the straight ramp from the first point to the
    /// slot, but the curve through them stays this far over the ground
    /// where it bulges, between the points as well as at them.
    pub clearance: f32,
    /// `TangentScale`: Bezier handles as a share of the neighbouring chord.
    pub handle: f32,
}

impl RoadShape {
    /// Where the work area goes for a slot at `at` turned to `slot_yaw`:
    /// `span` ground metres out, `side` off the slot's yaw.
    pub fn work_area_position(&self, field: &Heightfield, at: [f32; 3], slot_yaw: f32) -> [f32; 3] {
        let a = slot_yaw + self.side;
        let (x, z) = (at[0] + a.cos() * self.span, at[2] + a.sin() * self.span);
        [x, field.height([x, z]), z]
    }

    /// Where the path of a work area at `work`, turned to `yaw`, starts:
    /// its model's dummy in the world.
    pub fn start_at(&self, work: [f32; 3], yaw: f32) -> [f32; 3] {
        let (s, c) = yaw.sin_cos();
        let [fx, fy, fz] = self.start;
        [
            work[0] + fx * c - fz * s,
            work[1] + fy,
            work[2] + fx * s + fz * c,
        ]
    }

    /// The path from a work area at `work`, turned to `yaw`, to its slot
    /// at `slot`: from its dummy straight on the ground like shipped ones.
    pub fn road(
        &self,
        field: &Heightfield,
        work: [f32; 3],
        yaw: f32,
        slot: [f32; 3],
    ) -> Vec<PathPoint> {
        let from = self.start_at(work, yaw);
        let len = (slot[0] - from[0]).hypot(slot[2] - from[2]);
        let n = if self.spacing > 0. {
            ((len / self.spacing).round() as usize).max(1)
        } else {
            1
        };
        let ground: Vec<[f32; 2]> = (0..=n)
            .map(|k| {
                let t = k as f32 / n as f32;
                [
                    from[0] + (slot[0] - from[0]) * t,
                    from[2] + (slot[2] - from[2]) * t,
                ]
            })
            .collect();
        self.lay(field, &ground, from[1], slot[1])
    }

    /// A path through `ground` points (the first `start_y` high, the last
    /// on the slot, `slot_y` high): heights by the game's rule, then
    /// Bezier handles along the way, `o` towards the slot and `i` back,
    /// each a share of its own chord. The path edit mode's dragged points
    /// skip this and stay where they are put (`handles`).
    fn lay(
        &self,
        field: &Heightfield,
        ground: &[[f32; 2]],
        start_y: f32,
        slot_y: f32,
    ) -> Vec<PathPoint> {
        let n = ground.len().saturating_sub(1);
        let at = |k: usize| field.height(ground[k]);
        let Some(&[x0, z0]) = ground.first() else {
            return Vec::new();
        };
        if n == 0 {
            return vec![PathPoint {
                p: [x0, slot_y, z0],
                ..Default::default()
            }];
        }
        let y0 = start_y;
        let mut run = vec![0.];
        for k in 1..=n {
            let (a, b) = (ground[k - 1], ground[k]);
            run.push(run[k - 1] + (b[0] - a[0]).hypot(b[1] - a[1]));
        }
        let total = run[n].max(1e-4);
        let mut points: Vec<[f32; 3]> = (0..=n)
            .map(|k| {
                let y = match k {
                    0 => y0,
                    k if k == n => slot_y,
                    k => (y0 + (slot_y - y0) * run[k] / total).max(at(k) + self.clearance),
                };
                [ground[k][0], y, ground[k][1]]
            })
            .collect();
        self.clear(field, &mut points);
        self.handles(&points)
    }

    /// Lifts the inner `points` until the curve through them keeps
    /// `clearance` over the ground between them too. Over a convex brink,
    /// such as the crest a mine is placed on, the curve between two points
    /// 1.6 m apart cut under the ground, so the rail's track sank into it;
    /// the game would hide it under the ground the same way. Shipped rails
    /// rarely dip: the lowest point of the track between their ends is a
    /// median 0.14 m over the ground, and 32 of 2,113 dip under it, so laid
    /// ones keep to that. The first point stays on the work area's dummy
    /// and the last on the slot; over the first and last spans the
    /// clearance blends from the end's own height. Shipped paths and
    /// dragged points never come through here.
    fn clear(&self, field: &Heightfield, points: &mut [[f32; 3]]) {
        let n = points.len();
        if n < 3 || self.clearance <= 0. {
            return;
        }
        // Under 0.1 m a step at 1800's 1.6 m spacing.
        const STEPS: usize = 16;
        // Lifting a point moves its handles too, so the curve shifts a
        // little after each pass; the brinks tried settle in 2 or 3.
        const PASSES: usize = 8;
        // The ends are fixed, so the clearance blends in from each end's
        // own height over the ground: a cave's dummy can start inside the
        // hill (a tenth of shipped ones do), and asking for the full
        // clearance right beside it lifted the next point several metres.
        let over = |p: [f32; 3]| (p[1] - field.height([p[0], p[2]])).min(self.clearance);
        let ends = [over(points[0]), over(points[n - 1])];
        let mut lift = vec![0f32; n];
        for _ in 0..PASSES {
            lift.fill(0.);
            let path = self.handles(points);
            for k in 0..n - 1 {
                let (a, b) = (&path[k], &path[k + 1]);
                let o = a.o.unwrap_or_default();
                let i = b.i.unwrap_or_default();
                let c = [
                    a.p,
                    std::array::from_fn(|x| a.p[x] + o[x]),
                    std::array::from_fn(|x| b.p[x] + i[x]),
                    b.p,
                ];
                for s in 1..STEPS {
                    let t = s as f32 / STEPS as f32;
                    let u = 1. - t;
                    let w = [u * u * u, 3. * t * u * u, 3. * t * t * u, t * t * t];
                    let q: [f32; 3] = std::array::from_fn(|x| (0..4).map(|j| w[j] * c[j][x]).sum());
                    let over = match k {
                        0 => ends[0] + (self.clearance - ends[0]) * t,
                        k if k + 2 == n => ends[1] + (self.clearance - ends[1]) * u,
                        _ => self.clearance,
                    };
                    let need = field.height([q[0], q[2]]) + over - q[1];
                    if need <= 1e-4 {
                        continue;
                    }
                    // How far lifting each end raises the curve here: the
                    // point carries its handle's control point with it.
                    let carry = [w[0] + w[1], w[2] + w[3]];
                    let free = |e: usize| (1..n - 1).contains(&(k + e));
                    let total: f32 = (0..2).filter(|&e| free(e)).map(|e| carry[e]).sum();
                    // A quarter at least: near a fixed end the free one
                    // barely moves the curve, and the ground there is the
                    // end's own.
                    let by = need / total.max(0.25);
                    for e in (0..2).filter(|&e| free(e)) {
                        lift[k + e] = lift[k + e].max(by);
                    }
                }
            }
            if lift.iter().all(|&l| l == 0.) {
                return;
            }
            for (p, l) in points.iter_mut().zip(&lift) {
                p[1] += l;
            }
        }
    }

    /// A path through `points` as they are, with Bezier handles by the
    /// engine's rule (`rail::handles`); the game rebuilds them the same way
    /// on load. The path edit mode moves points in 3D and calls this alone.
    pub fn handles(&self, points: &[[f32; 3]]) -> Vec<PathPoint> {
        crate::island::objects::bezier_handles(points, self.handle)
    }

    /// Points a placed work area at its slot: a yaw `facing` off the
    /// direction to it, then a new road from its dummy. A template copied
    /// from another island would still point at that island's slot.
    pub fn link(&self, work: &mut Object, field: &Heightfield, slot: [f32; 3]) {
        let at = work.position;
        let yaw = (slot[2] - at[2]).atan2(slot[0] - at[0]) + self.facing;
        work.set_direction(yaw.rem_euclid(std::f32::consts::TAU));
        work.path = Some(self.road(field, at, work.direction, slot));
    }
}

/// `AreaIDs` keys every shipped pool island uses: the construction area
/// players build in, and the base area (cliffs, rims, nearby sea).
pub const CONSTRUCTION: u32 = 0x2001;
pub const BASE: u32 = 0x0001;

/// What the slot tool places: a slot and, for mines and rivers, its work
/// area.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
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

    /// The resource whose slot (random or fixed) `piece` is.
    pub fn of_slot(piece: Piece) -> Option<Resource> {
        Resource::ALL
            .into_iter()
            .map(|(r, _)| r)
            .find(|r| r.pieces(false).0 == piece || r.pieces(true).0 == piece)
    }

    /// Slot and work area pieces; `fixed` slots are always active. Marsh
    /// slots have neither a fixed variant nor a work area; 1800 Old World
    /// mines have two (a library without the cave record places one).
    pub fn pieces(self, fixed: bool) -> (Piece, &'static [Piece]) {
        const MINE: &[Piece] = &[Piece::MineWorkArea, Piece::CaveWorkArea];
        match (self, fixed) {
            (Resource::Mine, false) => (Piece::MineSlot, MINE),
            (Resource::Mine, true) => (Piece::FixedMineSlot, MINE),
            (Resource::Clay, _) => (Piece::ClaySlot, &[]),
            (Resource::Oil, _) => (Piece::OilSlot, &[]),
            (Resource::Gas, _) => (Piece::GasSlot, &[Piece::MineWorkArea]),
            (Resource::NewWorldMine, _) => {
                (Piece::NewWorldMineSlot, &[Piece::NewWorldMineWorkArea])
            }
            (Resource::River, false) => (Piece::RiverSlot, &[Piece::RiverWorkArea]),
            (Resource::River, true) => (Piece::FixedRiverSlot, &[Piece::RiverWorkArea]),
            (Resource::Marsh, _) => (Piece::MarshSlot, &[]),
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

/// Template records for each piece, from the province's shipped islands.
pub struct Library {
    templates: HashMap<Piece, Template>,
    slot_kinds: HashMap<u32, u16>,
}

impl Library {
    /// Reads `game`'s islands of `region`, pool islands first, until every
    /// piece has a record. Only `gamedata.data` is decoded, ~50 ms per
    /// island. The records are the a7m object store's: a game with its own
    /// codec, or without the shared pieces (`Harbours`), gets an empty
    /// library.
    pub fn load(
        game: &dyn Module,
        install: &Install,
        assets: &Assets,
        region: &str,
    ) -> Result<Library> {
        let islands = game.islands().context("the game has no islands")?;
        let rules = islands.rules();
        let Some(harbours) = islands
            .harbours()
            .filter(|_| islands.codec().a7m().is_some())
        else {
            return Ok(Library {
                templates: HashMap::new(),
                slot_kinds: HashMap::new(),
            });
        };
        let mut paths: Vec<String> = install
            .search(".a7m", 10_000)
            .into_iter()
            .map(|id| install.file_path(id).to_owned())
            .filter(|p| p.ends_with(".a7m") && rules.island_region(p) == region)
            .collect();
        // Large islands carry the start coast; try them first.
        paths.sort_by_key(|p| (!rules.is_pool(p), !rules.is_large(p)));
        let required = harbours.required_pieces(region);
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
            let pool = rules.is_pool(&path);
            if !pool
                && Piece::ALL
                    .iter()
                    .all(|p| p.pool_only() || templates.contains_key(p))
            {
                break;
            }
            let read = || -> Result<Objects> {
                let archive = Archive::parse(install.read(&path)?, &path)?;
                let gamedata = Adf::parse(&archive.read_path("gamedata.data")?)?;
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
                // The New World's mines are its region's alone: elsewhere
                // its records are none of the region's pieces.
                if let Some(piece) = islands.piece_of(&template, &asset.name)
                    && !templates.contains_key(&piece)
                    && (pool || !piece.pool_only())
                    && (!piece.regional() || required.contains(&piece))
                {
                    templates.insert(piece, store.template(i));
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

    /// New work areas of `pieces` for `slot`, each laid by its kind's
    /// shape (`harbours`' roads) and linked to it. Pieces the
    /// library has no record of are left out. Each lands in `store` too,
    /// so later IDs stay unique; the caller restores `store`.
    pub fn place_work_areas(
        &self,
        store: &mut Objects,
        areas: Option<&Layer>,
        field: &Heightfield,
        harbours: &dyn Harbours,
        pieces: &[Piece],
        slot: &Object,
    ) -> Vec<Object> {
        let mut out = Vec::new();
        for &piece in pieces {
            let shape = harbours.road_shape(Some(piece));
            let position = shape.work_area_position(field, slot.position, slot.direction);
            let Some(mut w) = self.place(store, piece, areas, position) else {
                continue;
            };
            shape.link(&mut w, field, slot.position);
            store.objects.push(w.clone());
            out.push(w);
        }
        out
    }
}

/// Template of an asset, following `BaseAssetGUID` for derived ones such as
/// "Fixed Slot Roman Mountain".
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
