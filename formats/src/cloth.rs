// Cloth: what a cfg's `<Clothes>` entries name and how the game moves it.
// Owns the entries' fields, the simulated mesh built from a cloth rdm, and
// a CPU step that follows the game's `CClothSimulation` (build 26043928):
// Verlet integration, tethers to the pinned vertices, Jacobi distance
// constraints over the mesh edges. docs/cloth-format.md has the evidence.
use std::collections::BinaryHeap;

use crate::model::{BoneLink, Transform, mesh_path, placement, texture_path};
use crate::rdm::Rdm;
use crate::xml::{Doc, Id};

/// One `<Clothes>` entry: a simulated rdm (sail, banner, flag, awning).
#[derive(Debug, Clone, PartialEq)]
pub struct Cloth {
    pub element: Id,
    pub name: String,
    /// Shipped `.rdm` path. Its vertex colours mark the pinned vertices
    /// ([`ClothMesh::build`]).
    pub mesh: String,
    /// Rdm space to the cfg's model space, as for a `<Models>` part.
    pub transform: Transform,
    /// `BONELINK_TRANSFORM`: hangs on a joint, `transform` then in its
    /// space. 6 of 371 entries in 117 (standards carried by units).
    pub link: Option<BoneLink>,
    /// Downward pull, `(0, -gravity, 0)` per second of frame time (0.05
    /// typical; 0 to 0.2 shipped). Not m/s²: see [`ClothState::step`].
    pub gravity: f32,
    /// Scales the wind on this cloth (0.05 to 2.5 shipped).
    pub wind_strength: f32,
    /// Radians about Y in the cloth's own space: the wind direction
    /// `(cos, 0, sin)` used when the cloth takes local wind.
    pub local_wind_direction: f32,
    pub allow_local_wind: bool,
    /// Off: every placed instance of the cfg shares one simulation.
    pub unique_simulation: bool,
    /// Pins let go when the object's damage passes the vertex alpha.
    pub damage_tearing: bool,
    /// Always 1 shipped. Read next to the rope segments; unused here.
    pub line_size: f32,
    /// Indices into the cfg's `<Collisions>` ([`colliders`]).
    pub collisions: Vec<u32>,
    pub material: ClothMaterial,
}

/// The `[Cloth]` material (ShaderID 0) of an entry, the fields a preview
/// draws. Texture paths are the shipped `_0.dds`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ClothMaterial {
    pub diffuse: Option<String>,
    pub normal: Option<String>,
    pub metallic: Option<String>,
    /// Grey mask where the player colour shows.
    pub dye_mask: Option<String>,
    pub color: [f32; 3],
    /// The player logo sits in the second UV set (`LOGO_ATLAS_ENABLED`).
    pub logo_atlas: bool,
}

/// A `<Collisions>` entry: a shape cloth is pushed out of.
#[derive(Debug, Clone, PartialEq)]
pub struct Collider {
    pub element: Id,
    pub name: String,
    pub shape: Shape,
    /// Model space, its scale scaling the shape.
    pub transform: Transform,
}

/// `CollisionType`, in the game's enum order (string table 0x1455CEA80).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Shape {
    /// The shape's XZ plane; cloth stays on its +Y side.
    Plane,
    /// Along the shape's Y axis, centred.
    Cylinder { length: f32, radius: f32 },
    /// Unit radius before the transform's scale. The cfg holds no size;
    /// that reading is ours.
    Sphere,
    /// Half extents, our reading.
    Box { extents: [f32; 3] },
}

/// The MAIN config's `<Clothes>` entries.
pub fn clothes(doc: &Doc) -> Vec<Cloth> {
    let Some(main) = main_config(doc) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for c in list(doc, main, "Clothes") {
        let num = |name: &str, or: f32| f32_of(doc, c, name).unwrap_or(or);
        let flag = |name: &str| text(doc, c, name) == "1";
        let collisions = doc
            .children(c)
            .filter(|&k| doc.name(k) == "CollisionIndex")
            .filter_map(|k| doc.text(k).parse().ok())
            .collect();
        out.push(Cloth {
            element: c,
            name: text(doc, c, "Name").to_string(),
            mesh: mesh_path(text(doc, c, "FileName")),
            transform: placement(doc, c),
            link: bone_link(doc, c),
            gravity: num("Gravity", 0.),
            wind_strength: num("WindStrength", 1.),
            local_wind_direction: num("LocalWindDirection", 0.),
            allow_local_wind: flag("AllowLocalWind"),
            unique_simulation: flag("UniqueSimulation"),
            damage_tearing: flag("DamageTearingEnabled"),
            line_size: num("LineSize", 1.),
            collisions,
            material: list(doc, c, "Materials")
                .next()
                .map(|m| material(doc, m))
                .unwrap_or_default(),
        });
    }
    out
}

/// The MAIN config's `<Collisions>`, in file order: [`Cloth::collisions`]
/// indexes this list.
pub fn colliders(doc: &Doc) -> Vec<Collider> {
    let Some(main) = main_config(doc) else {
        return Vec::new();
    };
    list(doc, main, "Collisions")
        .map(|c| {
            let num = |name: &str| f32_of(doc, c, name).unwrap_or(0.);
            let shape = match text(doc, c, "CollisionType") {
                "Cylinder" => Shape::Cylinder {
                    length: num("Length"),
                    radius: num("Radius"),
                },
                "Sphere" => Shape::Sphere,
                "Box" => Shape::Box {
                    extents: [num("Extents.x"), num("Extents.y"), num("Extents.z")],
                },
                _ => Shape::Plane,
            };
            Collider {
                element: c,
                name: text(doc, c, "Name").to_string(),
                shape,
                transform: placement(doc, c),
            }
        })
        .collect()
}

fn material(doc: &Doc, m: Id) -> ClothMaterial {
    let on = |name: &str| text(doc, m, name) == "1";
    // One shipped entry uses a model material (ShaderID 8, cModel* names).
    let tex = |enabled: &str, names: [&str; 2]| {
        let path = names
            .iter()
            .map(|n| text(doc, m, n))
            .find(|p| !p.is_empty());
        path.filter(|_| on(enabled)).map(texture_path)
    };
    ClothMaterial {
        diffuse: tex("DIFFUSE_ENABLED", ["cClothDiffuseTex", "cModelDiffTex"]),
        normal: tex("NORMAL_ENABLED", ["cClothNormalTex", "cModelNormalTex"]),
        metallic: tex(
            "METALLIC_TEX_ENABLED",
            ["cClothMetallicTex", "cModelMetallicTex"],
        ),
        dye_mask: tex("DYE_MASK_ENABLED", ["cClothDyeMask", "cModelDyeMask"]),
        color: ["r", "g", "b"].map(|k| f32_of(doc, m, &format!("cDiffuseColor.{k}")).unwrap_or(1.)),
        logo_atlas: on("LOGO_ATLAS_ENABLED"),
    }
}

fn bone_link(doc: &Doc, c: Id) -> Option<BoneLink> {
    let link = list(doc, c, "Transformer")
        .find(|&x| text(doc, x, "ConfigType") == "BONELINK_TRANSFORM")?;
    let model = text(doc, link, "ModelID").parse().ok()?;
    let bone = text(doc, link, "BoneHash").parse().ok()?;
    (bone != u32::MAX).then_some(BoneLink { model, bone })
}

fn main_config(doc: &Doc) -> Option<Id> {
    let configs = || doc.children(Id::ROOT).filter(|&c| doc.name(c) == "Config");
    configs()
        .find(|&c| text(doc, c, "ConfigType") == "MAIN")
        .or_else(|| configs().next())
}

// `<name><Config>…</Config>…</name>`
fn list<'a>(doc: &'a Doc, id: Id, name: &str) -> impl Iterator<Item = Id> + 'a {
    let parent = doc.child(id, name);
    parent
        .into_iter()
        .flat_map(|p| doc.children(p))
        .filter(|&c| doc.name(c) == "Config")
}

fn text<'a>(doc: &'a Doc, id: Id, name: &str) -> &'a str {
    doc.child(id, name).map_or("", |c| doc.text(c))
}

fn f32_of(doc: &Doc, id: Id, name: &str) -> Option<f32> {
    text(doc, id, name).parse().ok()
}

// ------------------------------------------------------------------ mesh

/// Bits of a vertex's flag word, from its first colour (`b0..b3` as
/// stored), as the game's loader (0x142308560) packs it.
mod flag {
    /// `b0 >= 128`, or the colour is white `(255, 255, 255)`.
    pub const PINNED: u32 = 1;
    /// `b2` in 126..=129: one side of a rope ribbon.
    pub const ROPE_KEEP: u32 = 2;
    /// `b2 == 255` and not white: the other side, merged onto the first.
    pub const ROPE_MERGE: u32 = 4;
    /// `b1 >> 4`, at bits 3..=6. Edges between groups one apart are
    /// marked; what reads the mark is not found.
    pub const GROUP: u32 = 0x78;
    /// `b3` at bits 7..=14: the damage level that frees a pin.
    pub const TEAR: u32 = 0x7f80;
}

fn vertex_flags(c: [u8; 4]) -> u32 {
    let [b0, b1, b2, b3] = c;
    let mut f = 0;
    if b2 == 255 && b1 == 255 && b0 == 255 {
        f |= flag::PINNED;
    } else {
        if b2 == 255 {
            f |= flag::ROPE_MERGE;
        } else if (126..=129).contains(&b2) {
            f |= flag::ROPE_KEEP;
        }
        f |= (b1 as u32 >> 1) & flag::GROUP;
        if b0 & 0x80 != 0 {
            f |= flag::PINNED;
        }
    }
    // b0 & 0x40 marks up to two vertices whose positions the loader keeps
    // as an axis; nothing in the step reads it.
    f | (b3 as u32) << 7
}

/// The simulated form of one cloth rdm: the game's LOD 0, the authored
/// mesh. (Near the camera the game also builds finer LODs by splitting
/// edges longer than `0.5 / 1.3^n` metres; this mesh does not.)
///
/// Drawn vertices are the rdm's own; vertices at one position weld into one
/// particle. All arrays are flat and indexed by `u32`.
#[derive(Debug, Clone, Default)]
pub struct ClothMesh {
    /// Drawn (rdm) vertex to particle.
    pub vertex_particle: Vec<u32>,
    /// Triangles over drawn vertices with the rope ribbons taken out: what
    /// the game draws. Drawing the rdm's own indices shows the same, the
    /// ribbons collapsing to zero width.
    pub triangles: Vec<u32>,
    /// Rope segments over drawn vertices: ribbons the loader collapsed to
    /// lines (sun sail ropes, washing lines). Simulated as edges; how the
    /// game draws them is not traced.
    pub lines: Vec<[u32; 2]>,
    /// Particles at rest, rdm space.
    rest: Vec<[f32; 3]>,
    pinned: Vec<u32>,
    edges: Vec<[u32; 2]>,
    rest_len2: Vec<f32>,
    /// Per particle: its nearest pin along the edges and that distance.
    tether_anchor: Vec<u32>,
    tether_len: Vec<f32>,
    /// Triangles over particles, for normals.
    faces: Vec<[u32; 3]>,
}

impl ClothMesh {
    /// Builds the simulated mesh the way the game's loader does
    /// (0x142308980): flags from the first vertex colour, rope ribbons
    /// merged to lines, coincident vertices welded, the unique edges as
    /// constraints and tethers from every particle to its nearest pin.
    /// A mesh without vertex colours is pinned everywhere and stays still.
    pub fn build(rdm: &Rdm) -> ClothMesh {
        let n = rdm.positions.len();
        let mut flags: Vec<u32> = match &rdm.colors {
            Some(c) => c.iter().map(|&c| vertex_flags(c)).collect(),
            None => vec![flag::PINNED; n],
        };
        let tris: Vec<[u32; 3]> = (rdm.indices.as_chunks::<3>().0.iter())
            .copied()
            .filter(|t| t.iter().all(|&i| (i as usize) < n))
            .collect();

        // Rope ribbons (0x14230A8E0): along each edge between a ROPE_KEEP
        // and a ROPE_MERGE vertex, the latter goes to its nearest such
        // neighbour. Triangles that lose a corner become line segments.
        let mut merge_to = vec![(u32::MAX, f32::MAX); n];
        for [a, b] in unique_edges(
            tris.iter()
                .flat_map(|t| [[t[0], t[1]], [t[0], t[2]], [t[1], t[2]]]),
        ) {
            let (keep, merge) = match (flags[a as usize], flags[b as usize]) {
                (fa, fb) if fa & flag::ROPE_KEEP != 0 && fb & flag::ROPE_MERGE != 0 => (a, b),
                (fa, fb) if fb & flag::ROPE_KEEP != 0 && fa & flag::ROPE_MERGE != 0 => (b, a),
                _ => continue,
            };
            let d = dist(rdm.positions[keep as usize], rdm.positions[merge as usize]);
            if d < merge_to[merge as usize].1 {
                merge_to[merge as usize] = (keep, d);
            }
        }
        let target = |i: u32| match merge_to[i as usize].0 {
            u32::MAX => i,
            t => t,
        };
        let mut triangles = Vec::with_capacity(tris.len() * 3);
        let mut lines = Vec::new();
        for t in &tris {
            let [a, b, c] = t.map(target);
            match (a == b, a == c, b == c) {
                (false, false, false) => triangles.extend([a, b, c]),
                (true, false, _) => lines.push([a.min(c), a.max(c)]),
                (false, true, _) | (false, false, true) => lines.push([a.min(b), a.max(b)]),
                _ => {}
            }
        }
        lines.sort_unstable();
        lines.dedup();

        // Weld (0x14A66B190): each vertex joins the first one at the same
        // position. Pins spread to the whole group; group and tear bits
        // take a set value over an unset one, later vertices winning.
        let mut order: Vec<u32> = (0..n as u32).filter(|&i| target(i) == i).collect();
        let key = |i: u32| rdm.positions[i as usize].map(|v| (v + 0.).to_bits());
        order.sort_unstable_by_key(|&i| (key(i), i));
        let mut first = vec![u32::MAX; n];
        for w in 0..order.len() {
            let i = order[w];
            first[i as usize] = match w {
                0 => i,
                _ if key(order[w - 1]) == key(i) => first[order[w - 1] as usize],
                _ => i,
            };
        }
        for i in 0..n {
            let j = first[i] as usize;
            if first[i] == u32::MAX || j == i {
                continue;
            }
            if (flags[i] | flags[j]) & flag::PINNED != 0 {
                flags[i] |= flag::PINNED;
                flags[j] |= flag::PINNED;
            }
            for mask in [flag::GROUP, flag::TEAR] {
                if flags[i] & mask != 0 {
                    flags[j] = flags[j] & !mask | flags[i] & mask;
                } else if flags[j] & mask != 0 {
                    flags[i] = flags[i] & !mask | flags[j] & mask;
                }
            }
        }

        // Particles: the welded representatives, in rdm order.
        let mut vertex_particle = vec![u32::MAX; n];
        let mut rest = Vec::new();
        let mut pflags = Vec::new();
        for i in 0..n {
            if first[i] == i as u32 {
                vertex_particle[i] = rest.len() as u32;
                rest.push(rdm.positions[i]);
                pflags.push(flags[i]);
            }
        }
        for i in 0..n {
            vertex_particle[i] = vertex_particle[first[target(i as u32) as usize] as usize];
        }
        let p = |i: u32| vertex_particle[i as usize];

        let edges: Vec<[u32; 2]> = unique_edges(
            (triangles.as_chunks::<3>().0.iter())
                .flat_map(|t| [[p(t[0]), p(t[1])], [p(t[0]), p(t[2])], [p(t[1]), p(t[2])]])
                .chain(lines.iter().map(|l| [p(l[0]), p(l[1])])),
        )
        .collect();
        let rest_len2: Vec<f32> = (edges.iter())
            .map(|&[a, b]| dist2(rest[a as usize], rest[b as usize]))
            .collect();
        let faces = (triangles.as_chunks::<3>().0.iter())
            .map(|t| [p(t[0]), p(t[1]), p(t[2])])
            .filter(|f| f[0] != f[1] && f[0] != f[2] && f[1] != f[2])
            .collect();
        let pinned: Vec<u32> = (0..rest.len() as u32)
            .filter(|&i| pflags[i as usize] & flag::PINNED != 0)
            .collect();
        let (tether_anchor, tether_len) = tethers(rest.len(), &edges, &rest_len2, &pinned);
        ClothMesh {
            vertex_particle,
            triangles,
            lines,
            rest,
            pinned,
            edges,
            rest_len2,
            tether_anchor,
            tether_len,
            faces,
        }
    }

    pub fn particle_count(&self) -> usize {
        self.rest.len()
    }

    /// Particles held at their rest position.
    pub fn pinned(&self) -> &[u32] {
        &self.pinned
    }

    /// Distance constraints between particles.
    pub fn edges(&self) -> &[[u32; 2]] {
        &self.edges
    }
}

// Sorted, without duplicates or self edges.
fn unique_edges(pairs: impl Iterator<Item = [u32; 2]>) -> impl Iterator<Item = [u32; 2]> {
    let mut keys: Vec<u64> = pairs
        .filter(|[a, b]| a != b)
        .map(|[a, b]| (a.min(b) as u64) << 32 | a.max(b) as u64)
        .collect();
    keys.sort_unstable();
    keys.dedup();
    keys.into_iter().map(|k| [(k >> 32) as u32, k as u32])
}

// Shortest distance along the edges from every particle to a pin, and
// which pin (0x14230B200). Pins tether to themselves at 0; particles no
// pin reaches get f32::MAX and are never pulled.
fn tethers(
    n: usize,
    edges: &[[u32; 2]],
    rest_len2: &[f32],
    pinned: &[u32],
) -> (Vec<u32>, Vec<f32>) {
    let mut start = vec![0u32; n + 1];
    for &[a, b] in edges {
        start[a as usize + 1] += 1;
        start[b as usize + 1] += 1;
    }
    for i in 0..n {
        start[i + 1] += start[i];
    }
    let mut fill = start.clone();
    let mut next = vec![(0u32, 0f32); edges.len() * 2];
    for (e, &[a, b]) in edges.iter().enumerate() {
        let len = rest_len2[e].sqrt();
        for (from, to) in [(a, b), (b, a)] {
            next[fill[from as usize] as usize] = (to, len);
            fill[from as usize] += 1;
        }
    }
    let mut anchor: Vec<u32> = (0..n as u32).collect();
    let mut len = vec![f32::MAX; n];
    // Nearest first: `Reverse` over the distance's bits, which order like
    // the floats since none is negative.
    let mut heap = BinaryHeap::new();
    for &p in pinned {
        len[p as usize] = 0.;
        heap.push((std::cmp::Reverse(0u32), p));
    }
    while let Some((std::cmp::Reverse(bits), i)) = heap.pop() {
        let d = f32::from_bits(bits);
        if d > len[i as usize] {
            continue;
        }
        for &(j, l) in &next[start[i as usize] as usize..start[i as usize + 1] as usize] {
            if d + l < len[j as usize] {
                len[j as usize] = d + l;
                anchor[j as usize] = anchor[i as usize];
                heap.push((std::cmp::Reverse((d + l).to_bits()), j));
            }
        }
    }
    (anchor, len)
}

// ------------------------------------------------------------------ step

/// Relaxation passes at LOD 0. The game runs `max(3, ⌊3·1.3^lod − 1⌋ − 1)`
/// passes, plus the one that measures the residual.
const PASSES: usize = 3;
/// The game caps a frame at 66 ms before simulating (0x14AF96260).
const MAX_DT: f32 = 0.066;
/// A step whose residual passes this starts again from rest.
const RESET_RESIDUAL: f32 = 0.2;
/// The game pulls a particle back once it is this much further from its
/// pin than the path along the cloth.
const TETHER_SLACK: f32 = 1.2;

/// One running simulation of a [`ClothMesh`]. Holds every buffer a step
/// touches, so [`ClothState::step`] does not allocate.
#[derive(Debug, Clone)]
pub struct ClothState {
    /// Rdm space to the space `wind` and the colliders are in (the cfg's
    /// model space): [`Cloth::transform`], composed with a joint's pose
    /// for a linked cloth. Its rotation turns forces into the cloth's
    /// space and its position sets the phase of the gusts. The cloth
    /// moves with it rigidly, as in the game.
    pub placement: Transform,
    /// Blow along [`Cloth::local_wind_direction`] instead of the world
    /// wind, without gravity. The game does this for instances with a
    /// flag (bit 25 at instance +0x108) whose source is not traced; ship
    /// sails are the likely users.
    pub local_wind: bool,
    gravity: f32,
    wind_strength: f32,
    local_wind_dir: [f32; 3],
    allow_local_wind: bool,
    colliders: Vec<(Shape, Transform)>,
    time: f32,
    residual: f32,
    pos: Vec<[f32; 3]>,
    prev: Vec<[f32; 3]>,
    normal: Vec<[f32; 3]>,
    /// 1 for pinned particles, 0 for the rest. The game lowers it to tear
    /// pins loose under damage.
    pin: Vec<f32>,
    delta: Vec<[f32; 3]>,
    drawn_pos: Vec<[f32; 3]>,
    drawn_normal: Vec<[f32; 3]>,
}

impl ClothState {
    /// At rest, in rdm space. `colliders` is the cfg's whole list; the
    /// entry's [`Cloth::collisions`] pick from it.
    pub fn new(mesh: &ClothMesh, cloth: &Cloth, colliders: &[Collider]) -> ClothState {
        let n = mesh.particle_count();
        let (s, c) = cloth.local_wind_direction.sin_cos();
        let mut pin = vec![0.; n];
        for &p in &mesh.pinned {
            pin[p as usize] = 1.;
        }
        let mut state = ClothState {
            placement: cloth.transform,
            local_wind: false,
            gravity: cloth.gravity,
            wind_strength: cloth.wind_strength,
            local_wind_dir: [c, 0., s],
            allow_local_wind: cloth.allow_local_wind,
            colliders: (cloth.collisions.iter())
                .filter_map(|&i| colliders.get(i as usize))
                .map(|c| (c.shape, c.transform))
                .collect(),
            time: 0.,
            residual: 0.,
            pos: Vec::new(),
            prev: Vec::new(),
            normal: vec![[0.; 3]; n],
            pin,
            delta: vec![[0.; 3]; n],
            drawn_pos: vec![[0.; 3]; mesh.vertex_particle.len()],
            drawn_normal: vec![[0.; 3]; mesh.vertex_particle.len()],
        };
        state.reset(mesh);
        state
    }

    /// Back to rest, as the game does when a step diverges.
    pub fn reset(&mut self, mesh: &ClothMesh) {
        self.pos.clone_from(&mesh.rest);
        self.prev.clone_from(&mesh.rest);
        self.residual = 0.;
        self.update_normals(mesh);
        self.write_drawn(mesh);
    }

    /// Advances by `dt` seconds of frame time under `wind`: the ambient
    /// wind in model space, its XZ length the preset's `WindStrength` (1.111
    /// in 76 of 117's 101 ambient presets) and its direction where it blows.
    ///
    /// The game steps once per frame with the frame's time, so the motion
    /// depends on the frame rate: gravity and wind add `force · dt` to a
    /// particle's per-frame velocity. The editor should call this once per
    /// drawn frame, like the game.
    pub fn step(&mut self, mesh: &ClothMesh, dt: f32, wind: [f32; 3]) {
        let dt = dt.clamp(0., MAX_DT);
        if dt == 0. || mesh.rest.is_empty() {
            return;
        }
        self.time += dt;
        if self.residual > RESET_RESIDUAL || self.residual.is_nan() {
            self.reset(mesh);
        }
        let (gravity, wind) = self.forces(dt, wind);
        let pos = &mut self.pos;

        // Verlet (0x14230C7D0); wind pushes along the normal by its
        // component on it.
        let particles = (pos.iter_mut().zip(&mut self.prev)).zip(self.normal.iter().zip(&self.pin));
        for ((p, prev), (&n, &pin)) in particles {
            let free = 1. - pin;
            let force = add(scale(gravity, dt), scale(n, dot(n, wind) * dt));
            let next = add(add(*p, sub(*p, *prev)), scale(force, free));
            *prev = *p;
            *p = next;
        }

        // Tethers: no further from the nearest pin than 1.2 times the
        // path along the cloth.
        let tethers = mesh.tether_anchor.iter().zip(&mesh.tether_len);
        for (i, (&anchor, &path)) in tethers.enumerate() {
            let d = sub(pos[anchor as usize], pos[i]);
            let len = dot(d, d).sqrt();
            let over = len - TETHER_SLACK * path;
            if over > 0. && len > 0. {
                pos[i] = add(pos[i], scale(d, over / len));
            }
        }

        // Jacobi passes over the edges with Jakobsen's square-root-free
        // correction, each end moved by s·d, the sum damped by 0.5^(30 dt).
        let weight = 0.5f32.powf(30. * dt);
        for pass in 0..=PASSES {
            self.delta.fill([0.; 3]);
            let mut squares = 0.;
            for (e, &[a, b]) in mesh.edges.iter().enumerate() {
                let l2 = mesh.rest_len2[e];
                let d = sub(pos[b as usize], pos[a as usize]);
                let s = (0.5 - l2 / (dot(d, d) + l2)) * 0.5;
                squares += s * s;
                let (a, b) = (a as usize, b as usize);
                self.delta[a] = add(self.delta[a], scale(d, s));
                self.delta[b] = sub(self.delta[b], scale(d, s));
            }
            if pass == PASSES {
                self.residual = (squares / pos.len() as f32).sqrt();
            }
            if !mesh.edges.is_empty() {
                for (p, &d) in pos.iter_mut().zip(&self.delta) {
                    *p = add(*p, scale(d, weight));
                }
            }
        }

        // Pins back to rest.
        for &i in &mesh.pinned {
            let (i, w) = (i as usize, self.pin[i as usize]);
            pos[i] = add(pos[i], scale(sub(mesh.rest[i], pos[i]), w));
        }

        // The game's collision response sits behind a virtual call not
        // traced. It hands it a thickness, `min(0.5 · longest edge, 0.325)`
        // at LOD 0, or 0 when the collider says so; with the thickness
        // banner_01's 1 cm pole would shove its whole banner aside, so this
        // keeps free particles out of the bare shapes.
        for &(shape, at) in &self.colliders {
            for (p, &pin) in pos.iter_mut().zip(&self.pin) {
                if pin > 0. {
                    continue;
                }
                let model = self.placement.apply(*p);
                if let Some(out) = push_out(shape, &at, model, 0.) {
                    *p = inverse_apply(&self.placement, out);
                }
            }
        }

        self.update_normals(mesh);
        self.write_drawn(mesh);
    }

    /// Per drawn (rdm) vertex, rdm space: draw with the part's usual
    /// placement.
    pub fn positions(&self) -> &[[f32; 3]] {
        &self.drawn_pos
    }

    /// Unit normals per drawn vertex, facing like the rdm's own.
    pub fn normals(&self) -> &[[f32; 3]] {
        &self.drawn_normal
    }

    /// Gravity and wind for this frame, in rdm space (0x142530B30).
    fn forces(&self, dt: f32, wind: [f32; 3]) -> ([f32; 3], [f32; 3]) {
        let t = self.time;
        let (gravity, w) = if self.local_wind && self.allow_local_wind {
            let gust = 1. + 0.2 * (0.8 * t).sin().powi(16) * (5. * t).sin().powi(2);
            let w = self.local_wind_dir.map(|v| 0.2 * gust * v);
            ([0.; 3], w)
        } else {
            let strength = (wind[0] * wind[0] + wind[2] * wind[2]).sqrt();
            let (nx, nz) = match strength > 0. {
                true => (wind[0] / strength, wind[2] / strength),
                false => (0., 0.),
            };
            let o = self.placement.position;
            let wx = (o[0] / 3. + 0.2 * t).sin().abs() * nx * strength * 0.05;
            let wz = (o[2] / 11. + 0.2 * t).cos().abs() * nz * strength * 0.05;
            let phase = nx * o[0] + nz * o[2] + 0.8 * t;
            let gust = 1. + 0.4 * phase.sin().abs().powi(16) * (5. * t).sin().powi(2);
            let inv = conjugate(self.placement.rotation);
            let g = rotate(inv, [0., -self.gravity, 0.]);
            (g, rotate(inv, [gust * wx, 0., gust * wz]))
        };
        // Long frames get weaker forces: full up to 1/30 s, 0.2 at 0.1 s.
        let f = ((dt - 0.1) / (-1. / 15.)).clamp(0., 1.);
        let k = f + (1. - f) * 0.2;
        (
            gravity.map(|v| v * k),
            w.map(|v| v * k * self.wind_strength),
        )
    }

    // Face normals summed per particle (0x14A68C810, 0x14A68FBC0).
    fn update_normals(&mut self, mesh: &ClothMesh) {
        self.normal.fill([0.; 3]);
        for f in &mesh.faces {
            let [a, b, c] = f.map(|i| self.pos[i as usize]);
            let n = normalize(cross(sub(b, a), sub(c, a)));
            for &i in f {
                self.normal[i as usize] = add(self.normal[i as usize], n);
            }
        }
        for n in &mut self.normal {
            *n = normalize(*n);
        }
    }

    fn write_drawn(&mut self, mesh: &ClothMesh) {
        for (v, &p) in mesh.vertex_particle.iter().enumerate() {
            self.drawn_pos[v] = self.pos[p as usize];
            self.drawn_normal[v] = self.normal[p as usize];
        }
    }
}

// The point moved just outside `shape` (placed at `at`, model space) by
// `radius`, or `None` when it is clear.
fn push_out(shape: Shape, at: &Transform, p: [f32; 3], radius: f32) -> Option<[f32; 3]> {
    let local = rotate(conjugate(at.rotation), sub(p, at.position));
    let s = at.scale;
    let moved = match shape {
        Shape::Plane => (local[1] < radius).then_some([local[0], radius, local[2]]),
        Shape::Sphere => {
            let r = s[0] + radius;
            let d = dot(local, local).sqrt();
            (d < r && d > 0.).then(|| local.map(|v| v * r / d))
        }
        Shape::Cylinder { length, radius: r } => {
            let r = r * s[0] + radius;
            let half = 0.5 * length * s[1];
            let d = (local[0] * local[0] + local[2] * local[2]).sqrt();
            (local[1].abs() <= half && d < r && d > 0.)
                .then(|| [local[0] * r / d, local[1], local[2] * r / d])
        }
        Shape::Box { extents } => {
            let e = [0, 1, 2].map(|k| extents[k] * s[k] + radius);
            let depth = [0, 1, 2].map(|k| e[k] - local[k].abs());
            match depth.iter().all(|&d| d > 0.) {
                false => None,
                true => {
                    let k = (0..3)
                        .min_by(|&a, &b| depth[a].total_cmp(&depth[b]))
                        .unwrap();
                    let mut out = local;
                    out[k] = e[k].copysign(local[k]);
                    Some(out)
                }
            }
        }
    }?;
    let world = rotate(at.rotation, moved);
    Some([0, 1, 2].map(|k| world[k] + at.position[k]))
}

fn inverse_apply(t: &Transform, p: [f32; 3]) -> [f32; 3] {
    let r = rotate(conjugate(t.rotation), sub(p, t.position));
    [0, 1, 2].map(|k| match t.scale[k] {
        0. => r[k],
        s => r[k] / s,
    })
}

fn conjugate(q: [f32; 4]) -> [f32; 4] {
    [-q[0], -q[1], -q[2], q[3]]
}

// v + 2w(u × v) + 2(u × (u × v)), u = q.xyz
fn rotate(q: [f32; 4], v: [f32; 3]) -> [f32; 3] {
    let u = [q[0], q[1], q[2]];
    let t = cross(u, v).map(|c| 2. * c);
    let c = cross(u, t);
    [0, 1, 2].map(|k| v[k] + q[3] * t[k] + c[k])
}

fn add(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [a[0] + b[0], a[1] + b[1], a[2] + b[2]]
}

fn scale(v: [f32; 3], s: f32) -> [f32; 3] {
    v.map(|c| c * s)
}

fn sub(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}

fn dot(a: [f32; 3], b: [f32; 3]) -> f32 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

fn cross(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}

fn normalize(v: [f32; 3]) -> [f32; 3] {
    let l = dot(v, v).sqrt();
    match l > 0. {
        true => v.map(|c| c / l),
        false => v,
    }
}

fn dist2(a: [f32; 3], b: [f32; 3]) -> f32 {
    let d = sub(a, b);
    dot(d, d)
}

fn dist(a: [f32; 3], b: [f32; 3]) -> f32 {
    dist2(a, b).sqrt()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vfs::{FileId, Install, install_dir};
    use rayon::prelude::*;

    const FRAME: f32 = 1. / 30.;

    // A `w`×`h` grid in the XY plane, `spacing` apart, hanging down from
    // its top row; `color` gives each vertex its colour.
    fn grid(w: u32, h: u32, spacing: f32, color: impl Fn(u32, u32) -> [u8; 4]) -> Rdm {
        let mut rdm = Rdm::default();
        for y in 0..h {
            for x in 0..w {
                rdm.positions
                    .push([x as f32 * spacing, -(y as f32) * spacing, 0.]);
                rdm.normals.push([0., 0., -1.]);
            }
        }
        rdm.colors = Some(
            (0..h)
                .flat_map(|y| (0..w).map(move |x| (x, y)))
                .map(|(x, y)| color(x, y))
                .collect(),
        );
        for y in 0..h - 1 {
            for x in 0..w - 1 {
                let i = y * w + x;
                rdm.indices
                    .extend([i, i + w, i + 1, i + 1, i + w, i + w + 1]);
            }
        }
        rdm
    }

    fn top_pinned(_: u32, y: u32) -> [u8; 4] {
        [if y == 0 { 128 } else { 0 }, 0, 0, 0]
    }

    fn cloth(gravity: f32, wind_strength: f32) -> Cloth {
        Cloth {
            element: Id::ROOT,
            name: String::new(),
            mesh: String::new(),
            transform: Transform::IDENTITY,
            link: None,
            gravity,
            wind_strength,
            local_wind_direction: 0.,
            allow_local_wind: true,
            unique_simulation: false,
            damage_tearing: false,
            line_size: 1.,
            collisions: Vec::new(),
            material: ClothMaterial::default(),
        }
    }

    #[test]
    fn vertex_colours_become_flags() {
        assert_eq!(vertex_flags([128, 0, 0, 0]), flag::PINNED);
        assert_eq!(vertex_flags([192, 0, 0, 0]), flag::PINNED);
        assert_eq!(vertex_flags([64, 0, 0, 0]), 0);
        assert_eq!(vertex_flags([255, 255, 255, 0]), flag::PINNED);
        assert_eq!(vertex_flags([0, 0, 128, 0]), flag::ROPE_KEEP);
        assert_eq!(vertex_flags([0, 0, 255, 0]), flag::ROPE_MERGE);
        assert_eq!(vertex_flags([0, 32, 0, 0]) & flag::GROUP, 2 << 3);
        assert_eq!(vertex_flags([0, 0, 0, 200]) & flag::TEAR, 200 << 7);
    }

    #[test]
    fn a_banner_hangs_from_its_pins() {
        let mesh = ClothMesh::build(&grid(4, 5, 0.25, top_pinned));
        assert_eq!(mesh.pinned().len(), 4);
        let mut state = ClothState::new(&mesh, &cloth(0.05, 1.), &[]);
        for _ in 0..90 {
            state.step(&mesh, FRAME, [0.; 3]);
        }
        let pos = state.positions();
        for (v, &p) in pos[..4].iter().enumerate() {
            assert!(dist(p, [v as f32 * 0.25, 0., 0.]) < 1e-6, "pins stay put");
        }
        // Soft constraints give a little under gravity, the tethers bound it.
        let bottom = pos[16][1];
        assert!(bottom < -1. && bottom > -TETHER_SLACK - 1e-3, "{bottom}");
        assert!(pos.iter().flatten().all(|v| v.is_finite()));
    }

    #[test]
    fn wind_pushes_along_the_normal() {
        let mesh = ClothMesh::build(&grid(4, 5, 0.25, top_pinned));
        let mut state = ClothState::new(&mesh, &cloth(0.01, 2.), &[]);
        // A wind with some Z: it blows through the banner's face.
        for _ in 0..60 {
            state.step(&mesh, FRAME, [0.3, 0., 1.]);
        }
        let z: f32 = state.positions()[4..].iter().map(|p| p[2]).sum();
        assert!(z > 0.01, "the free part leans downwind: {z}");
        let mut still = ClothState::new(&mesh, &cloth(0.01, 2.), &[]);
        for _ in 0..60 {
            still.step(&mesh, FRAME, [1., 0., 0.]);
        }
        let z: f32 = still.positions().iter().map(|p| p[2].abs()).sum();
        assert!(z < 1e-6, "wind in the banner's plane does not push it: {z}");
    }

    #[test]
    fn steps_are_deterministic() {
        let mesh = ClothMesh::build(&grid(5, 5, 0.2, top_pinned));
        let run = || {
            let mut s = ClothState::new(&mesh, &cloth(0.05, 1.), &[]);
            for k in 0..50 {
                s.step(&mesh, FRAME * (1. + (k % 3) as f32 * 0.1), [1.111, 0., 0.4]);
            }
            s.positions().to_vec()
        };
        assert_eq!(run(), run());
    }

    // A ribbon (two triangles, one long side 128 and the other 255) turns
    // into one rope segment; a seam vertex welds into its twin.
    #[test]
    fn ribbons_become_ropes_and_seams_weld() {
        let mut rdm = Rdm::default();
        rdm.positions = vec![
            [0., 0., 0.],
            [1., 0., 0.],
            [0., 0.01, 0.],
            [1., 0.01, 0.],
            [1., 0., 0.], // seam twin of vertex 1
            [1., -1., 0.],
        ];
        rdm.colors = Some(vec![
            [128, 0, 128, 0],
            [0, 0, 128, 0],
            [0, 0, 255, 0],
            [0, 0, 255, 0],
            [0, 0, 0, 0],
            [0, 0, 0, 0],
        ]);
        rdm.indices = vec![0, 2, 1, 1, 2, 3, 4, 5, 0];
        let mesh = ClothMesh::build(&rdm);
        assert_eq!(mesh.lines, vec![[0, 1]]);
        assert_eq!(mesh.triangles, vec![4, 5, 0]);
        assert_eq!(mesh.particle_count(), 3, "0, 1 with its twin, 5");
        assert_eq!(mesh.vertex_particle, vec![0, 1, 0, 1, 1, 2]);
        assert_eq!(mesh.edges(), &[[0, 1], [0, 2], [1, 2]]);
        assert_eq!(mesh.pinned(), &[0]);
    }

    #[test]
    fn colliders_push_cloth_out() {
        let at = Transform::IDENTITY;
        let pushed = push_out(Shape::Sphere, &at, [0.5, 0., 0.], 0.1).unwrap();
        assert!((pushed[0] - 1.1).abs() < 1e-6);
        let cylinder = Shape::Cylinder {
            length: 2.,
            radius: 0.1,
        };
        assert_eq!(
            push_out(cylinder, &at, [0., 2., 0.05], 0.),
            None,
            "past its end"
        );
        let pushed = push_out(cylinder, &at, [0., 0.5, 0.05], 0.).unwrap();
        assert!((pushed[2] - 0.1).abs() < 1e-6);
        let pushed = push_out(Shape::Plane, &at, [3., -1., 2.], 0.).unwrap();
        assert_eq!(pushed, [3., 0., 2.]);
        let b = Shape::Box {
            extents: [1., 1., 1.],
        };
        assert_eq!(push_out(b, &at, [0.9, 0.2, 0.], 0.), Some([1., 0.2, 0.]));
    }

    // Every shipped cloth entry: its mesh builds, hangs under gravity and
    // the typical wind for two seconds without diverging, pins in place.
    #[test]
    fn every_shipped_cloth_simulates() {
        for (var, want_entries) in [("ANNO117_DIR", 371), ("ANNO1800_DIR", 1251)] {
            let Some(dir) = install_dir(var) else {
                eprintln!("skip: {var} unset");
                continue;
            };
            let install = Install::open(dir, Install::DATA_FIRST).unwrap();
            // (entries, meshes built, meshes with a pin, collision indices
            // past the cfg's list) per cfg.
            let found: Vec<[usize; 4]> = (0..install.file_count())
                .into_par_iter()
                .map(|i| FileId(i as u32))
                .filter(|&id| install.file_name(id).ends_with(".cfg"))
                .filter_map(|id| {
                    let path = install.file_path(id);
                    if !path.starts_with("data/") {
                        return None;
                    }
                    let bytes = install.read_file(id).ok()?;
                    memchr::memmem::find(&bytes, b"<Clothes>")?;
                    let doc = Doc::parse(&bytes);
                    let (entries, colliders) = (clothes(&doc), colliders(&doc));
                    let (mut built, mut pinned, mut dangling) = (0, 0, 0);
                    for c in &entries {
                        dangling += (c.collisions.iter())
                            .filter(|&&i| i as usize >= colliders.len())
                            .count();
                        let Ok(bytes) = install.read(&c.mesh) else {
                            continue;
                        };
                        let rdm = Rdm::parse(&bytes).unwrap();
                        let mesh = ClothMesh::build(&rdm);
                        let mut state = ClothState::new(&mesh, c, &colliders);
                        for _ in 0..60 {
                            state.step(&mesh, FRAME, [1.111, 0., 0.]);
                        }
                        let pos = state.positions();
                        assert_eq!(pos.len(), rdm.positions.len());
                        assert!(
                            pos.iter().flatten().all(|v| v.is_finite()),
                            "{path} {}",
                            c.mesh
                        );
                        for &p in mesh.pinned() {
                            let off = dist(state.pos[p as usize], mesh.rest[p as usize]);
                            assert!(off < 1e-4, "{path}: pin {p} {off} from rest");
                        }
                        built += 1;
                        pinned += !mesh.pinned().is_empty() as usize;
                    }
                    Some([entries.len(), built, pinned, dangling])
                })
                .collect();
            let sum = |k: usize| found.iter().map(|f| f[k]).sum::<usize>();
            let (entries, built, pinned) = (sum(0), sum(1), sum(2));
            eprintln!(
                "{var}: {} cfgs, {entries} entries, {built} meshes, {pinned} pinned, \
                 {} collision indices past the list",
                found.len(),
                sum(3)
            );
            assert_eq!(entries, want_entries);
            // 117 misses one mesh; every mesh that ships is pinned somewhere.
            assert!(built + 1 >= entries, "{built} of {entries}");
            assert_eq!(pinned, built);
        }
    }

    // On a flat banner the computed normals at rest face like the rdm's
    // own, and the sun sail's rope ribbons turn into rope segments.
    #[test]
    fn shipped_banner_and_sun_sail() {
        let Some(dir) = install_dir("ANNO117_DIR") else {
            eprintln!("skip: ANNO117_DIR unset");
            return;
        };
        let install = Install::open(dir, Install::DATA_FIRST).unwrap();
        let read = |p: &str| Rdm::parse(&install.read(p).unwrap()).unwrap();
        let banner = read(
            "data/base/graphics/library/props/decoration/banner/rdm/banner_playercolor_01.rdm",
        );
        let mesh = ClothMesh::build(&banner);
        assert_eq!(mesh.pinned().len(), 4);
        let state = ClothState::new(&mesh, &cloth(0.05, 0.4), &[]);
        for (n, stored) in state.normals().iter().zip(&banner.normals) {
            assert!(dot(*n, *stored) > 0.99, "{n:?} {stored:?}");
        }
        let sail = read(
            "data/base/graphics/library/modular_level_kits/caeso_island/rdm/sunsail_shape01.rdm",
        );
        let mesh = ClothMesh::build(&sail);
        // 8 pinned vertices, the two sides of 4 rope ends.
        assert_eq!(mesh.pinned().len(), 4);
        assert!(!mesh.lines.is_empty());
        assert_eq!(
            mesh.triangles.len() / 3,
            303 - 52,
            "52 ribbon triangles become rope"
        );
        let cfg = install
            .read("data/base/graphics/library/props/decoration/banner/banner_01.cfg")
            .unwrap();
        let doc = Doc::parse(&cfg);
        let c = &clothes(&doc)[0];
        assert_eq!(
            c.mesh,
            "data/base/graphics/library/props/decoration/banner/rdm/banner_playercolor_01.rdm"
        );
        assert_eq!(
            (c.gravity, c.wind_strength, c.collisions.as_slice()),
            (0.05, 0.4, &[0][..])
        );
        assert_eq!(
            colliders(&doc)[0].shape,
            Shape::Cylinder {
                length: 1.,
                radius: 0.01
            }
        );
    }
}
