// Owns how an island looks: the material palette, a baked ground-color
// texture (material maps × material diffuse textures), the shipped terrain
// meshes as a read-only reference layer, and the water plane. The heightfield
// is the terrain; the save path regenerates the meshes from it.
//
// The game splats up to six materials per texel in a shader. The editor's
// renderer takes one texture per mesh, so the splat is baked on the CPU at
// `BAKE_PER_METRE` and every ground tile samples it through grid UVs.
//
// Not used yet: `tintmaps/` (mostly neutral grey with purple/blue fringes, so
// not a plain color multiplier; encoding unknown) and `normalmaps/*.ctt`.
use anno_formats::{island, tmc::TileMesh, vfs::Install};
use anyhow::{Context, Result};
use gpui_editor_kit::{
    gpu::{self, ControlMap, Mesh, MeshData, Splat},
    heightfield::Rect,
    texture::{Compressed, Dds, Image, Releasable, Texture},
};
use std::{collections::BTreeMap, sync::Arc};

/// Output texels per metre. Only cliff tops and the Map/Height fallbacks use
/// the bake now (terrain splats on the GPU), so 4 is plenty and 4× cheaper
/// than the matalpha resolution.
pub const BAKE_PER_METRE: usize = 4;
const BAKE_MAX: usize = 4096;
/// Cliff mesh level: the finest that ships geometry (rd3d `MinMeshLevel`).
const MESH_LEVEL: u32 = 2;
/// Ground textures and normal maps on the GPU, px per repeat. Shipped ones
/// are 1024-2048; at the usual 4 m repeat 1024 is 256 px per metre, about
/// what a close view resolves. A set's 25-30 layers cost ~40 MB as BC7 with
/// mips, normals as much again. The old 128 looked blurry next to the game.
const MATERIAL_MIP: u32 = 1024;
/// Decoded preview mip for the CPU bake and palette thumbnails.
const PREVIEW_MIP: u32 = 128;
/// Splat control texels: one per matalpha texel (1800: 2/m, 117: 4-8/m),
/// so the GPU interpolates the game's own grid; duplicating texels made
/// stair steps. 2048² caps a 512 m island at 64 MB for both pairs.
const CONTROL_MAX: usize = 2048;

/// Triangles steeper than this (normal y below it) are walls and get rock.
const WALL_NORMAL_Y: f32 = 0.55;

/// Read access to a level-0 material map (`matid` words or `matalpha`
/// words): a decoded grid, or the editor's chunked copy while painting.
pub trait MaterialWords: Sync {
    fn size(&self) -> [usize; 2];
    fn word(&self, x: usize, y: usize) -> u32;
}

impl MaterialWords for island::Grid<u32> {
    fn size(&self) -> [usize; 2] {
        [self.width, self.height]
    }

    fn word(&self, x: usize, y: usize) -> u32 {
        self.get(x, y)
    }
}

/// One palette entry: diffuse texture and how many metres one repeat covers.
pub struct Material {
    pub file: String,
    /// Compressed with its mip chain, as the GPU samples it.
    pub texture: Image,
    /// Tangent-space normal map (`NormalFileName`), same treatment.
    pub normal: Option<Image>,
    /// A small decoded mip for CPU work: the cliff-top bake and thumbnails.
    pub preview: Arc<Texture>,
    pub repetition: f32,
    /// `AlphaSharpness`: how far the diffuse alpha (a height) cuts into the
    /// material's coverage where it is laid over the ones below.
    sharpness: f32,
}

/// The island's material set (`MaterialSetFileName`), by material id.
pub struct Palette {
    pub materials: BTreeMap<u8, Material>,
    /// Ids with a material, bottom to top. The game bakes ground one pass
    /// per material in set file order, each alpha-blended over the ones
    /// before (`terrain_baking.fx`, `BlendSplat`), so a texel's weights are
    /// coverages, not shares: the latest full-weight material hides the rest.
    pub stack: Vec<u8>,
    /// Per id: stack rank and sharpness, for per-texel work.
    layers: [Option<(u8, f32)>; 32],
    /// Every material's id and `<GroundType>` (Grass, Bushes, Soil, …;
    /// empty or `None` grows no props), in file order: the order the
    /// game's prop scatter composites them in.
    pub ground_layers: Vec<(u8, String)>,
}

/// First `<Tag>value</>` (Anno's short-closed XML) or `<Tag>value</Tag>`.
pub fn tag<'a>(xml: &'a str, name: &str) -> Option<&'a str> {
    let open = format!("<{name}>");
    let rest = &xml[xml.find(&open)? + open.len()..];
    rest.find("</").map(|end| rest[..end].trim())
}

/// A material xml's diffuse texture for the GPU (compressed, mips, top
/// capped at `gpu_max`), a decoded preview mip, and its normal map for the
/// GPU when it ships one. Sources are named `.psd`/`.png`; the game ships
/// `<stem>_0.dds`.
fn diffuse(
    install: &Install,
    shared: Option<&Arc<Install>>,
    material_xml: &str,
    gpu_max: u32,
) -> Result<(Image, Texture, Option<Image>)> {
    let material = String::from_utf8_lossy(&install.read(material_xml)?).into_owned();
    let path = |tag_name: &str| -> Result<String> {
        let source = tag(&material, tag_name)
            .with_context(|| format!("no {tag_name}"))?
            .replace('\\', "/");
        let stem = source.rsplit_once('.').map_or(source.as_str(), |(s, _)| s);
        Ok(format!("{stem}_0.dds"))
    };
    // The texels go once on the GPU when the app's install can read them
    // again (`Releasable`): the set's layers and normals held 70 MB.
    let compressed = move |install: &Install, path: &str| -> Option<Compressed> {
        let bytes = install.read(path).ok()?;
        let mut c = Dds::parse(&bytes).ok()?.compressed(&bytes).ok()?;
        c.shrink(gpu_max);
        Some(c)
    };
    let image = |c: Compressed, path: String| -> Image {
        match shared {
            Some(shared) => {
                let install = shared.clone();
                Releasable::new(c.clone(), move || compressed(&install, &path))
                    .map_or_else(|_| Image::from(c), Image::from)
            }
            None => Image::from(c),
        }
    };
    let diffuse = path("DiffuseFileName")?;
    let bytes = install.read(&diffuse)?;
    let header = Dds::parse(&bytes)?;
    let mut preview = header.decode(&bytes, header.mip_for(PREVIEW_MIP))?;
    // Diffuse alpha carries other data (height/gloss), not coverage.
    preview
        .rgba
        .iter_mut()
        .skip(3)
        .step_by(4)
        .for_each(|a| *a = 255);
    let texture = match header.compressed(&bytes) {
        Ok(mut c) => {
            c.shrink(gpu_max);
            image(c, diffuse)
        }
        // Uncompressed DDS: upload the capped mip as RGBA.
        Err(_) => Image::from(header.decode(&bytes, header.mip_for(gpu_max))?),
    };
    let normal = path("NormalFileName")
        .ok()
        .and_then(|normal| Some(image(compressed(install, &normal)?, normal)));
    Ok((texture, preview, normal))
}

impl Palette {
    /// The material set `set`; with `shared`, the app's install, its
    /// layer textures let their texels go once uploaded.
    pub fn load(install: &Install, shared: Option<&Arc<Install>>, set: &str) -> Result<Self> {
        let xml = String::from_utf8_lossy(&install.read(set)?).into_owned();
        let mut materials = BTreeMap::new();
        let mut ground_layers = Vec::new();
        let mut stack = Vec::new();
        let mut blocks = Vec::new();
        for block in xml.split("<Material>").skip(1) {
            let (Some(id), Some(file)) = (tag(block, "ID"), tag(block, "FileName")) else {
                continue;
            };
            let Ok(id) = id.parse::<u8>() else { continue };
            if id >= 32 || file.is_empty() {
                continue;
            }
            ground_layers.push((id, tag(block, "GroundType").unwrap_or_default().to_owned()));
            blocks.push((id, file, block));
        }
        // Each material reads 2-11 MB of DDS; one thread per material keeps
        // a set of 30 near the time of its largest.
        let textures: Vec<_> = std::thread::scope(|scope| {
            let jobs: Vec<_> = (blocks.iter())
                .map(|&(_, file, _)| {
                    scope.spawn(move || diffuse(install, shared, file, MATERIAL_MIP))
                })
                .collect();
            jobs.into_iter()
                .map(|j| j.join().expect("material texture thread"))
                .collect()
        });
        for ((id, file, block), loaded) in blocks.into_iter().zip(textures) {
            let number = |name: &str, default: f32| {
                tag(block, name)
                    .and_then(|r| r.parse::<f32>().ok())
                    .unwrap_or(default)
            };
            match loaded {
                Ok((texture, preview, normal)) => {
                    stack.push(id);
                    materials.insert(
                        id,
                        Material {
                            file: file.to_owned(),
                            texture,
                            normal,
                            preview: Arc::new(preview),
                            repetition: number("Repetition", 4.).max(0.25),
                            sharpness: number("AlphaSharpness", 1.).max(0.),
                        },
                    );
                }
                Err(e) => eprintln!("material {id} ({file}): {e:#}"),
            }
        }
        let mut layers = [None; 32];
        for (rank, id) in stack.iter().enumerate() {
            layers[usize::from(*id)] = Some((rank as u8, materials[id].sharpness));
        }
        Ok(Self {
            materials,
            stack,
            layers,
            ground_layers,
        })
    }

    /// How much of a texel each of its six slots shows once the game has
    /// laid them over each other (see `stack`).
    pub fn visible(&self, ids: [u8; 6], weights: [u8; 6]) -> [f32; 6] {
        let mut cover = [(0u8, 0f32); 6];
        for k in 0..6 {
            cover[k] = (ids[k] & 31, f32::from(weights[k]) / 31.);
        }
        let n = self.shares(&mut cover);
        let mut out = [0f32; 6];
        for &(id, v) in &cover[..n] {
            // A repeated id shows once, in its first slot.
            if let Some(k) = (0..6).find(|&k| ids[k] & 31 == id && weights[k] > 0) {
                out[k] = v;
            }
        }
        out
    }

    /// Turns (id, coverage 0..1) into (id, share shown) in place, bottom to
    /// top, and returns how many entries remain at the front. Each material
    /// is laid over the ones below with alpha
    /// `saturate(s·(a − 1) + (s + 1)·coverage)`, the diffuse alpha `a` taken
    /// as a mid 0.5; the lowest is opaque. Repeats merge; ids outside the
    /// set drop.
    pub fn shares(&self, cover: &mut [(u8, f32)]) -> usize {
        let key = |c: &(u8, f32)| match self.rank(c.0) {
            Some(rank) if c.1 > 0. => rank,
            _ => u8::MAX,
        };
        cover.sort_unstable_by_key(key);
        let mut n = 0;
        for i in 0..cover.len() {
            if key(&cover[i]) == u8::MAX {
                break;
            }
            if n > 0 && cover[n - 1].0 == cover[i].0 {
                cover[n - 1].1 += cover[i].1;
            } else {
                cover[n] = cover[i];
                n += 1;
            }
        }
        for i in 0..n {
            let (_, s) = self.layers[usize::from(cover[i].0 & 31)].expect("ranked");
            let alpha = if i == 0 {
                1.
            } else {
                (-0.5 * s + (s + 1.) * cover[i].1).clamp(0., 1.)
            };
            for c in &mut cover[..i] {
                c.1 *= 1. - alpha;
            }
            cover[i].1 = alpha;
        }
        n
    }

    /// The 5-bit weight that lays material `id` over what is below at
    /// `alpha`, inverting [`Palette::visible`].
    pub fn coverage_for(&self, id: u8, alpha: f32) -> u8 {
        let s = self.layers[usize::from(id & 31)].map_or(0., |l| l.1);
        if alpha >= 0.999 {
            return 31;
        }
        if alpha <= 0. {
            return 0;
        }
        (((alpha + 0.5 * s) / (s + 1.)).clamp(0., 1.) * 31.).round() as u8
    }

    /// Stack rank of `id`, `None` when the set has no such material.
    pub fn rank(&self, id: u8) -> Option<u8> {
        self.layers[usize::from(id & 31)].map(|l| l.0)
    }

    /// The rock used on cliff walls: the set's first cliff material.
    pub fn rock(&self) -> Option<&Material> {
        self.materials
            .values()
            .find(|m| m.file.contains("/rock/cliff"))
            .or_else(|| self.materials.values().find(|m| m.file.contains("/rock/")))
            .or_else(|| {
                self.materials
                    .values()
                    .find(|m| m.file.contains("mountain") || m.file.contains("cliff"))
            })
    }

    /// The GPU splat for the level-0 material maps: palette layers, plus two
    /// index/weight control pairs holding the six Anno slots per texel
    /// (slots 0-3 in the first pair, 4-5 in the second).
    pub fn splat(&self, ids: &island::Grid<u32>, alphas: &island::Grid<u32>) -> Result<Splat> {
        let (mut layers, mut tiling, mut sharpness) = (Vec::new(), Vec::new(), Vec::new());
        let mut normals = Vec::new();
        for m in self.stack.iter().map(|id| &self.materials[id]) {
            normals.extend(m.normal.clone());
            // Compressed with mips; the renderer brings mixed sizes to the
            // smallest via mip offsets.
            layers.push(m.texture.clone());
            tiling.push(m.repetition);
            sharpness.push(m.sharpness);
        }
        anyhow::ensure!(!layers.is_empty(), "material set has no usable textures");
        let side = alphas.size()[0].clamp(1, CONTROL_MAX);
        let whole = Rect {
            x0: 0,
            z0: 0,
            x1: side,
            z1: side,
        };
        let [i0, w0, i1, w1] = self
            .control_planes(ids, alphas, side, whole)
            .map(|p| ControlMap::new(side, side, p).map(Arc::new));
        // Normals light every layer or none: the kit takes one per layer.
        if normals.len() != layers.len() {
            if !normals.is_empty() {
                eprintln!(
                    "material set: {} of {} layers ship normal maps; ground drawn without",
                    normals.len(),
                    layers.len()
                );
            }
            normals.clear();
        }
        Ok(Splat {
            layers,
            tiling,
            sharpness,
            normals,
            indices: vec![i0?, i1?],
            weights: vec![w0?, w1?],
        })
    }

    /// Recomputes the control texels of `splat` under `area` (island metres
    /// x0, z0, x1, z1) from the material maps, after a paint dab. The GPU
    /// picks the change up on its next frame.
    pub fn update_splat(
        &self,
        splat: &Splat,
        ids: &impl MaterialWords,
        alphas: &impl MaterialWords,
        size: f32,
        area: [f32; 4],
    ) {
        let side = splat.indices[0].size()[0];
        let per_metre = side as f32 / size;
        let texel = |v: f32| ((v * per_metre).max(0.) as usize).min(side);
        let rect = Rect {
            x0: texel(area[0]),
            z0: texel(area[1]),
            x1: (texel(area[2]) + 1).min(side),
            z1: (texel(area[3]) + 1).min(side),
        };
        if rect.is_empty() {
            return;
        }
        let [i0, w0, i1, w1] = self.control_planes(ids, alphas, side, rect);
        splat.indices[0].write(rect, &i0);
        splat.weights[0].write(rect, &w0);
        splat.indices[1].write(rect, &i1);
        splat.weights[1].write(rect, &w1);
    }

    /// The four control planes (index A, weight A, index B, weight B; RGBA
    /// rows) of control texels `rect` on a `side`² map. Layers follow
    /// `stack` order, as in [`Palette::splat`], which is the order the kit
    /// lays them over each other.
    fn control_planes(
        &self,
        ids: &impl MaterialWords,
        alphas: &impl MaterialWords,
        side: usize,
        rect: Rect,
    ) -> [Vec<u8>; 4] {
        let ([iw, ih], [aw, ah]) = (ids.size(), alphas.size());
        // Material id to layer, NO_LAYER where the set has none: six
        // lookups per texel, millions of texels.
        const NO_LAYER: u8 = u8::MAX;
        let mut layer_of = [NO_LAYER; 256];
        for (i, &id) in self.stack.iter().enumerate() {
            layer_of[usize::from(id)] = i as u8;
        }
        let per_texel = aw as f32 / side as f32;
        let (width, rows) = (rect.x1 - rect.x0, rect.z1 - rect.z0);
        // Index A, weight A, index B, weight B: RGBA rows each, filled in
        // bands of rows on every core.
        let mut planes: [Vec<u8>; 4] = std::array::from_fn(|_| vec![0u8; width * rows * 4]);
        let threads = std::thread::available_parallelism().map_or(4, |n| n.get());
        let band = rows.div_ceil(threads).max(1) * width * 4;
        let [ia, wa, ib, wb] = &mut planes;
        std::thread::scope(|scope| {
            let bands = (ia.chunks_mut(band).zip(wa.chunks_mut(band)))
                .zip(ib.chunks_mut(band).zip(wb.chunks_mut(band)));
            for (n, ((ia, wa), (ib, wb))) in bands.enumerate() {
                let layer_of = &layer_of;
                scope.spawn(move || {
                    let z0 = rect.z0 + n * band / (width * 4);
                    for t in 0..ia.len() / 4 {
                        let (x, y) = (rect.x0 + t % width, z0 + t / width);
                        let ax = (((x as f32 + 0.5) * per_texel) as usize).min(aw - 1);
                        let ay = (((y as f32 + 0.5) * per_texel) as usize).min(ah - 1);
                        let slots = island::unpack_slots(
                            ids.word((ax / 2).min(iw - 1), (ay / 2).min(ih - 1)),
                        );
                        let weights = island::unpack_slots(alphas.word(ax, ay));
                        for k in 0..6 {
                            let layer = layer_of[usize::from(slots[k])];
                            let alpha = weights[k];
                            if layer == NO_LAYER || alpha == 0 {
                                continue;
                            }
                            let (index, weight) = if k < 4 {
                                (&mut *ia, &mut *wa)
                            } else {
                                (&mut *ib, &mut *wb)
                            };
                            index[t * 4 + k % 4] = layer;
                            // 5-bit weights to 8 bits.
                            weight[t * 4 + k % 4] = alpha * 8 + alpha / 4;
                        }
                    }
                });
            }
        });
        planes
    }

    /// Bakes ground color at `BAKE_PER_METRE` over `size` metres from the
    /// level-0 material maps (`ids`: one word per 2×2 alpha texels).
    pub fn bake(
        &self,
        ids: &island::Grid<u32>,
        alphas: &island::Grid<u32>,
        size: f32,
    ) -> Result<Texture> {
        let side = (size as usize * BAKE_PER_METRE).clamp(1, BAKE_MAX);
        let per_metre = side as f32 / size;
        let alpha_per_texel = alphas.width as f32 / side as f32;
        let mut rgba = vec![0u8; side * side * 4];
        par_rows(&mut rgba, side * 4, |y, row| {
            for (x, out) in row.chunks_mut(4).enumerate() {
                let ax = (((x as f32 + 0.5) * alpha_per_texel) as usize).min(alphas.width - 1);
                let ay = (((y as f32 + 0.5) * alpha_per_texel) as usize).min(alphas.height - 1);
                let slots = island::unpack_slots(
                    ids.get((ax / 2).min(ids.width - 1), (ay / 2).min(ids.height - 1)),
                );
                let shown = self.visible(slots, island::unpack_slots(alphas.get(ax, ay)));
                let world = [(x as f32 + 0.5) / per_metre, (y as f32 + 0.5) / per_metre];
                let mut color = [0f32; 3];
                let mut total = 0.;
                for k in (0..6).filter(|&k| shown[k] > 0.) {
                    let m = &self.materials[&(slots[k] & 31)];
                    let s = m
                        .preview
                        .sample([world[0] / m.repetition, world[1] / m.repetition]);
                    for c in 0..3 {
                        color[c] += f32::from(s[c]) * shown[k];
                    }
                    total += shown[k];
                }
                if total == 0. {
                    color = [96., 90., 80.];
                }
                for c in 0..3 {
                    out[c] = color[c].clamp(0., 255.) as u8;
                }
                out[3] = 255;
            }
        });
        Texture::from_rgba(side as u32, side as u32, rgba)
    }
}

/// Runs `f(y, row)` over the `row`-byte rows of `buf` on every core.
fn par_rows(buf: &mut [u8], row: usize, f: impl Fn(usize, &mut [u8]) + Sync) {
    let rows = buf.len() / row.max(1);
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get());
    let per = rows.div_ceil(threads).max(1);
    let f = &f;
    std::thread::scope(|scope| {
        for (i, chunk) in buf.chunks_mut(per * row).enumerate() {
            scope.spawn(move || {
                for (k, r) in chunk.chunks_mut(row).enumerate() {
                    f(i * per + k, r);
                }
            });
        }
    });
}

/// The shipped terrain meshes (finest level) in island metres. They cover
/// the island's whole active area, not just cliffs, and are drawn as a
/// reference: once heights change they are stale until saved and remeshed.
pub struct ShippedMeshes {
    tiles: Vec<TileMesh>,
    size: f32,
}

impl ShippedMeshes {
    /// None at all, for islands created in the editor.
    pub fn empty(size: f32) -> Self {
        Self {
            tiles: Vec::new(),
            size,
        }
    }

    pub fn load(install: &Install, island_path: &str, size: f32) -> Self {
        let mut cliffs = Self {
            tiles: Vec::new(),
            size,
        };
        let Ok(base) = crate::island::island_data_dir(island_path) else {
            return cliffs;
        };
        for path in install.list(&format!("{base}/meshes/{MESH_LEVEL}")) {
            match install.read(&path).and_then(|b| TileMesh::parse(&b)) {
                Ok(tile) if !tile.indices.is_empty() => {
                    cliffs.tiles.push(tile);
                }
                Ok(_) => {}
                Err(e) => eprintln!("{path}: {e:#}"),
            }
        }
        cliffs
    }

    pub fn len(&self) -> usize {
        self.tiles.len()
    }

    /// Triangles of every tile, built into meshes or not.
    pub fn triangles(&self) -> usize {
        self.tiles.iter().map(|t| t.indices.len() / 3).sum()
    }

    /// Two meshes per tile: flat-ish tops sample the ground texture from
    /// above, walls get rock projected from the side they face (per triangle,
    /// so vertices are not shared). Without textures both fall back to colors.
    pub fn tile_meshes(
        &self,
        index: usize,
        ground: Option<&Arc<Texture>>,
        rock: Option<&Material>,
    ) -> Vec<Arc<Mesh>> {
        let mut out = Vec::new();
        {
            let tile = &self.tiles[index];
            let mut top = MeshData {
                material: gpu::Material::Lit,
                texture: ground.cloned().map(Into::into),
                ..Default::default()
            };
            let mut wall = MeshData {
                material: gpu::Material::Lit,
                texture: rock.map(|r| r.texture.clone()),
                // Rock diffuse alpha is gloss, not coverage.
                ignore_alpha: true,
                ..Default::default()
            };
            for tri in tile.indices.as_chunks::<3>().0 {
                let p = tri.map(|i| tile.positions[i as usize]);
                let n = normal(p);
                let is_wall = n[1].abs() < WALL_NORMAL_Y;
                let mesh = if is_wall { &mut wall } else { &mut top };
                let base = mesh.positions.len() as u32;
                for v in p {
                    mesh.positions.push(v);
                    if is_wall {
                        mesh.colors.push(if mesh.texture.is_some() {
                            0xffffff
                        } else {
                            0x8a8273
                        });
                        if let Some(r) = rock {
                            // Project onto the vertical plane the wall faces most.
                            let along = if n[0].abs() > n[2].abs() { v[2] } else { v[0] };
                            mesh.uvs.push([along / r.repetition, -v[1] / r.repetition]);
                        }
                    } else {
                        mesh.colors.push(if mesh.texture.is_some() {
                            0xffffff
                        } else {
                            0xb3a27e
                        });
                        if ground.is_some() {
                            mesh.uvs.push([v[0] / self.size, v[2] / self.size]);
                        }
                    }
                }
                mesh.indices.extend([base, base + 1, base + 2]);
            }
            for mesh in [top, wall] {
                if mesh.indices.is_empty() {
                    continue;
                }
                match Mesh::new(mesh) {
                    Ok(m) => out.push(Arc::new(m)),
                    Err(e) => eprintln!("cliff mesh: {e:#}"),
                }
            }
        }
        out
    }
}

fn normal(p: [[f32; 3]; 3]) -> [f32; 3] {
    let (a, b) = (
        gpui_editor_kit::math::sub(p[1], p[0]),
        gpui_editor_kit::math::sub(p[2], p[0]),
    );
    gpui_editor_kit::math::normalize(gpui_editor_kit::math::cross(a, b)).unwrap_or([0., 1., 0.])
}

/// Linear color of the open sea the viewport's sky draws past `water`'s
/// quad: the quad's own color decoded from sRGB, so where the quad blends
/// over it nothing changes and the edge does not show.
pub const SEA_BODY: [f32; 3] = [0.011, 0.0685, 0.147];

/// A flat sea at y = 0 reaching well past the island, like the game's
/// ocean; past it the viewport sky's sea (`SEA_BODY`) goes on.
pub fn water(size: f32) -> Arc<Mesh> {
    let (lo, hi) = (-size, size * 2.);
    Arc::new(
        Mesh::new(MeshData {
            positions: vec![[lo, 0., lo], [hi, 0., lo], [hi, 0., hi], [lo, 0., hi]],
            colors: vec![0xffffff; 4],
            indices: vec![0, 2, 1, 0, 3, 2],
            material: gpu::Material::Blend(gpu::Blend {
                // Deep blue, as the game's open water.
                color: 0x1b4a6b,
                opacity: 0.78,
                // Metres of depth over which the shore fades in from clear.
                fade: 3.,
                fresnel: 0.5,
                // Metres of view depth past which the floor no longer
                // shows, as in the game's open water.
                depth: 14.,
            }),
            ..Default::default()
        })
        .expect("water quad"),
    )
}

#[cfg(test)]
mod tests {
    use super::{normal, tag};

    #[test]
    fn reads_short_closed_xml() {
        let xml = "<Material><ID>7</>\n<FileName>a/b.xml</>\n<Repetition>4.0</Repetition></>";
        assert_eq!(tag(xml, "ID"), Some("7"));
        assert_eq!(tag(xml, "FileName"), Some("a/b.xml"));
        assert_eq!(tag(xml, "Repetition"), Some("4.0"));
        assert_eq!(tag(xml, "Missing"), None);
    }

    #[test]
    fn wall_test_uses_face_normals() {
        let flat = normal([[0., 0., 0.], [0., 0., 1.], [1., 0., 0.]]);
        let wall = normal([[0., 0., 0.], [1., 0., 0.], [0., 1., 0.]]);
        assert!(flat[1].abs() > 0.99 && wall[1].abs() < 0.01);
    }
}
