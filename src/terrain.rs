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
use anno_core::island::Stack;
use anno_formats::{
    a7m::{grids, island, tmc::TileMesh},
    vfs::Install,
    xml::tag,
};
use anno_render::a7m::{Grit, Relief, Splat};
use anyhow::{Context, Result};
use gpui_editor_kit::{
    gpu::{self, ControlMap, Mesh, MeshData},
    heightfield::Rect,
    texture::{Compressed, Dds, Image, Releasable, Texture},
};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

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

    /// Row `y`'s words over columns `xs`, appended to `out`: a run at a
    /// time where the layout allows.
    fn row_into(&self, y: usize, xs: std::ops::Range<usize>, out: &mut Vec<u32>) {
        out.extend(xs.map(|x| self.word(x, y)));
    }
}

impl MaterialWords for island::Grid<u32> {
    fn size(&self) -> [usize; 2] {
        [self.width, self.height]
    }

    fn word(&self, x: usize, y: usize) -> u32 {
        self.get(x, y)
    }

    fn row_into(&self, y: usize, xs: std::ops::Range<usize>, out: &mut Vec<u32>) {
        out.extend_from_slice(&self.data[y * self.width..][xs]);
    }
}

/// One palette entry: diffuse texture and how many metres one repeat covers.
/// Clones share the textures.
#[derive(Clone)]
pub struct Material {
    pub file: String,
    /// Compressed with its mip chain, as the GPU samples it.
    pub texture: Image,
    /// Tangent-space normal map (`NormalFileName`), same treatment.
    pub normal: Option<Image>,
    /// Glossiness in red (`SpecFileName`), same treatment.
    pub gloss: Option<Image>,
    /// Height in red (`HeightFileName`), same treatment: the game bakes it
    /// into its ground's displacement and the normals it lights it by
    /// (`anno_render::a7m::Relief`).
    pub height: Option<Image>,
    /// `DisplacementFloor`, `DisplacementCeil` (world units) and 1 with
    /// `DisplacementAdd`.
    pub displacement: [f32; 3],
    /// A small decoded mip for CPU work: the cliff-top bake and thumbnails.
    pub preview: Arc<Texture>,
    pub repetition: f32,
    /// `AlphaSharpness`: how far the diffuse alpha (a height) cuts into the
    /// material's coverage where it is laid over the ones below.
    sharpness: f32,
}

/// The island's material set (`MaterialSetFileName`), by material id.
/// Clones share the textures.
#[derive(Clone)]
pub struct Palette {
    pub materials: BTreeMap<u8, Material>,
    /// Ids with a material, bottom to top, with their sharpness.
    pub stack: Stack,
    /// Every material's id and `<GroundType>` (Grass, Bushes, Soil, …;
    /// empty or `None` grows no props), in file order: the order the
    /// game's prop scatter composites them in.
    pub ground_layers: Vec<(u8, String)>,
    /// The set's grit (`GritDiffuseTexture` and its tiling, sharpness and
    /// slope), laid over the ground where the island's grit map asks.
    pub grit: Option<GritSource>,
    /// Per id: how far the game sinks ground of a material it displaces
    /// wholly below the surface (Albion's marsh floor and marsh islands),
    /// the middle of its `DisplacementFloor`..`DisplacementCeil`; 0 for the
    /// rest. See [`Palette::sinking`].
    sink: [f32; 32],
}

/// A material sinks the ground when its displacement range lies wholly
/// below this, metres. Mud (-0.08..-0.03) and rock (-0.3..0.8) do not; the
/// celtic marsh floor (-0.57..-0.505) and marsh islands (-0.325..-0.295) do.
const SINKS_BELOW: f32 = -0.1;

/// A material set's grit, before an island's mask is known.
#[derive(Clone)]
pub struct GritSource {
    pub texture: Image,
    pub tiling: f32,
    pub sharpness: f32,
    pub slope: f32,
}

/// A material file's `.psd`/`.png` source name as the game ships it,
/// `<stem>_0.dds`.
fn shipped(source: &str) -> String {
    let source = source.replace('\\', "/");
    let stem = source.rsplit_once('.').map_or(source.as_str(), |(s, _)| s);
    format!("{stem}_0.dds")
}

/// The grit map as the splat's mask: its value in red, one texel per cell
/// (0.5 m), rows along +Z.
fn grit_mask(layer: &grids::Layer) -> Result<ControlMap> {
    ControlMap::new(
        layer.width,
        layer.height,
        grit_texels(layer, (0..layer.width, 0..layer.height)),
    )
}

/// RGBA texels of the grit map's cells in `cells` (columns, rows), for a
/// [`ControlMap::write`] after a dab.
pub fn grit_texels(
    layer: &grids::Layer,
    (xs, ys): (std::ops::Range<usize>, std::ops::Range<usize>),
) -> Vec<u8> {
    let mut out = Vec::with_capacity(xs.len() * ys.len() * 4);
    for y in ys {
        for x in xs.clone() {
            out.extend([layer.get(x, y).min(255) as u8, 0, 0, 255]);
        }
    }
    out
}

/// What a material xml gives the GPU: its diffuse texture (compressed,
/// mips, top capped at `gpu_max`), a decoded preview mip, and its normal
/// and gloss maps when it ships them.
struct Loaded {
    texture: Image,
    preview: Texture,
    normal: Option<Image>,
    gloss: Option<Image>,
    height: Option<Image>,
}

fn diffuse(
    install: &Install,
    shared: Option<&Arc<Install>>,
    material_xml: &str,
    gpu_max: u32,
) -> Result<Loaded> {
    let material = String::from_utf8_lossy(&install.read(material_xml)?).into_owned();
    let path = |tag_name: &str| -> Result<String> {
        let source = tag(&material, tag_name).with_context(|| format!("no {tag_name}"))?;
        Ok(shipped(source))
    };
    // The texels go once on the GPU when the app's install can read them
    // again (`Releasable`): the set's layers and normals held 70 MB.
    // `red`: maps read in red alone keep it alone (`Compressed::red`).
    let compressed = move |install: &Install, path: &str, red: bool| -> Option<Compressed> {
        let c = capped(install, path, gpu_max).ok()?.1.ok()?;
        Some(if red { c.red() } else { c })
    };
    // Validated first so the texels move in: a copy for the fallback
    // doubled a set's 100 MB while it loaded.
    let image = |c: Compressed, path: String, red: bool| -> Image {
        match shared {
            Some(shared) if c.validate().is_ok() => {
                let install = shared.clone();
                // Every level: ground layers upload whole.
                Releasable::new(c, move |_| compressed(&install, &path, red))
                    .expect("validated")
                    .into()
            }
            _ => Image::from(c),
        }
    };
    let diffuse = path("DiffuseFileName")?;
    let (header, read) = capped(install, &diffuse, gpu_max)?;
    let (mut preview, texture) = match read {
        Ok(c) => {
            let level = (0..c.mips.len())
                .find(|&l| c.mip_size(l).0.max(c.mip_size(l).1) <= PREVIEW_MIP)
                .unwrap_or(c.mips.len() - 1);
            (c.decode(level)?, image(c, diffuse, false))
        }
        // Uncompressed DDS: upload the capped mip as RGBA.
        Err(bytes) => (
            header.decode(&bytes, header.mip_for(PREVIEW_MIP))?,
            Image::from(header.decode(&bytes, header.mip_for(gpu_max))?),
        ),
    };
    // Diffuse alpha carries other data (height/gloss), not coverage.
    preview
        .rgba
        .iter_mut()
        .skip(3)
        .step_by(4)
        .for_each(|a| *a = 255);
    let map = |tag_name: &str, red: bool| {
        path(tag_name)
            .ok()
            .and_then(|path| Some(image(compressed(install, &path, red)?, path, red)))
    };
    Ok(Loaded {
        texture,
        preview,
        normal: map("NormalFileName", false),
        // The splat reads gloss in red alone: BC4, half the BC7 shipped
        // (21 MB for the continental island's 32 layers).
        gloss: map("SpecFileName", true),
        height: map("HeightFileName", false),
    })
}

/// DDS `path`'s mips from the first at most `max` texels a side, and its
/// header. Read whole, a ground layer's top levels were most of its 2-11
/// MB and went again at `shrink`; where the archive stores the file as is,
/// only the levels kept are read. The whole file when it is not
/// block-compressed, to decode.
fn capped(install: &Install, path: &str, max: u32) -> Result<(Dds, Result<Compressed, Vec<u8>>)> {
    // Header and DX10 extension.
    let header = Dds::parse(&install.read_range(path, 0, 148)?)?;
    // Headers sometimes count past 1x1 (`Dds::compressed_from`).
    let chain = (header.mips).min(32 - header.width.max(header.height).leading_zeros());
    let first = header.mip_for(max).min(chain.saturating_sub(1));
    let tail = install.read_range(path, header.mip_offset(first) as u64, u64::MAX)?;
    if let Ok(c) = header.compressed_from(first, &tail) {
        return Ok((header, Ok(c)));
    }
    // Not block-compressed, or cut short above the levels kept.
    let bytes = install.read(path)?;
    let header = Dds::parse(&bytes)?;
    Ok(match header.compressed(&bytes) {
        Ok(mut c) => {
            c.shrink(max);
            (header, Ok(c))
        }
        Err(_) => (header, Err(bytes)),
    })
}

impl Material {
    fn new(
        file: &str,
        loaded: Loaded,
        repetition: f32,
        sharpness: f32,
        displacement: [f32; 3],
    ) -> Self {
        Self {
            file: file.to_owned(),
            texture: loaded.texture,
            normal: loaded.normal,
            gloss: loaded.gloss,
            height: loaded.height,
            displacement,
            preview: Arc::new(loaded.preview),
            repetition: repetition.max(0.25),
            sharpness: sharpness.max(0.),
        }
    }
}

impl Palette {
    /// The material set `set`; with `shared`, the app's install, its
    /// layer textures let their texels go once uploaded.
    pub fn load(install: &Install, shared: Option<&Arc<Install>>, set: &str) -> Result<Self> {
        let xml = String::from_utf8_lossy(&install.read(set)?).into_owned();
        let mut materials = BTreeMap::new();
        let mut ground_layers = Vec::new();
        let mut stack = Stack::default();
        let mut blocks = Vec::new();
        let mut sink = [0f32; 32];
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
                Ok(loaded) => {
                    let (repetition, sharpness) =
                        (number("Repetition", 4.), number("AlphaSharpness", 1.));
                    stack.push(id, sharpness);
                    let (floor, ceil) = (
                        number("DisplacementFloor", 0.),
                        number("DisplacementCeil", 0.),
                    );
                    if ceil < SINKS_BELOW {
                        sink[usize::from(id)] = (floor + ceil) / 2.;
                    }
                    let add = number("DisplacementAdd", 0.);
                    materials.insert(
                        id,
                        Material::new(file, loaded, repetition, sharpness, [floor, ceil, add]),
                    );
                }
                Err(e) => eprintln!("material {id} ({file}): {e:#}"),
            }
        }
        // The set's header, before its first material.
        let header = xml.split("<Material>").next().unwrap_or("");
        let grit = tag(header, "GritDiffuseTexture")
            .filter(|t| !t.is_empty())
            .and_then(|t| {
                let path = shipped(t);
                let bytes = install.read(&path).ok()?;
                let mut c = Dds::parse(&bytes).ok()?.compressed(&bytes).ok()?;
                c.shrink(MATERIAL_MIP);
                let number = |name: &str, default: f32| {
                    tag(header, name)
                        .and_then(|v| v.parse().ok())
                        .unwrap_or(default)
                };
                Some(GritSource {
                    texture: Image::from(c),
                    tiling: number("GritTiling", 4.).max(0.25),
                    sharpness: number("GritAlphaSharpness", 5.).max(0.),
                    slope: number("GritSlope", 0.2),
                })
            });
        Ok(Self {
            materials,
            stack,
            ground_layers,
            grit,
            sink,
        })
    }

    /// A set with no materials, to add to one by one.
    pub fn empty() -> Self {
        Self {
            materials: BTreeMap::new(),
            stack: Stack::default(),
            ground_layers: Vec::new(),
            grit: None,
            sink: [0.; 32],
        }
    }

    /// Adds material `file`, read from `install`, at free `id` on top of the
    /// stack (last in the set file). `install` may be the other game's, for a
    /// material ported into a mod that isn't mounted yet. `ground_type` is the
    /// set's `<GroundType>`.
    pub fn add_material(
        &mut self,
        install: &Install,
        id: u8,
        file: &str,
        (repetition, sharpness): (f32, f32),
        ground_type: &str,
    ) -> Result<()> {
        anyhow::ensure!(
            id < 32 && !self.materials.contains_key(&id),
            "id {id} is taken"
        );
        let loaded = diffuse(install, None, file, MATERIAL_MIP)?;
        // No range until the set file gives it one: flat under relief.
        let flat = [0.; 3];
        (self.materials).insert(id, Material::new(file, loaded, repetition, sharpness, flat));
        self.stack.push(id, sharpness);
        self.ground_layers.push((id, ground_type.to_owned()));
        Ok(())
    }

    /// Puts material `file` at `id` in place of the one there, keeping its
    /// place in the stack: ground painted with `id` shows the new texture.
    pub fn replace_material(
        &mut self,
        install: &Install,
        id: u8,
        file: &str,
        (repetition, sharpness): (f32, f32),
        ground_type: &str,
    ) -> Result<()> {
        self.rank(id).context("no such material")?;
        let loaded = diffuse(install, None, file, MATERIAL_MIP)?;
        // The slot's displacement range stays: the set file keeps it.
        let range = self.materials.get(&id).map_or([0.; 3], |m| m.displacement);
        (self.materials).insert(
            id,
            Material::new(file, loaded, repetition, sharpness, range),
        );
        self.stack.set_sharpness(id, sharpness);
        if let Some(g) = self.ground_layers.iter_mut().find(|g| g.0 == id) {
            g.1 = ground_type.to_owned();
        }
        Ok(())
    }

    /// Takes material `id` out of the set; the ones above it move down a
    /// rank.
    pub fn remove_material(&mut self, id: u8) {
        self.materials.remove(&id);
        self.stack.remove(id);
        self.ground_layers.retain(|g| g.0 != id);
    }

    /// See [`Stack::visible`].
    pub fn visible(&self, ids: [u8; 6], weights: [u8; 6]) -> [f32; 6] {
        self.stack.visible(ids, weights)
    }

    /// See [`Stack::shares`].
    pub fn shares(&self, cover: &mut [(u8, f32)]) -> usize {
        self.stack.shares(cover)
    }

    /// See [`Stack::rank`].
    pub fn rank(&self, id: u8) -> Option<u8> {
        self.stack.rank(id)
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
    /// (slots 0-3 in the first pair, 4-5 in the second), and the set's grit
    /// over the island's grit map (`GritMap2`) when both exist, blended the
    /// the way `bake` says the island's game bakes ground.
    ///
    /// The control maps draw from `ids` and `alphas` (shared, not copied)
    /// when the renderer reads them; the returned planes take the maps
    /// again after edits (`Palette::update_splat`).
    pub fn splat<M: MaterialWords + Clone + Send + 'static>(
        &self,
        bake: anno_core::game::Bake,
        ids: &M,
        alphas: &M,
        grit: Option<&grids::Layer>,
    ) -> Result<(Splat, Arc<SplatPlanes<M>>)> {
        let (mut layers, mut tiling, mut sharpness) = (Vec::new(), Vec::new(), Vec::new());
        let (mut normals, mut gloss) = (Vec::new(), Vec::new());
        let (mut heights, mut ranges) = (Vec::new(), Vec::new());
        for m in self.stack.iter().map(|id| &self.materials[id]) {
            normals.extend(m.normal.clone());
            gloss.extend(m.gloss.clone());
            heights.extend(m.height.clone());
            ranges.push(m.displacement);
            // Compressed with mips; the renderer brings mixed sizes to the
            // smallest via mip offsets.
            layers.push(m.texture.clone());
            tiling.push(m.repetition);
            sharpness.push(m.sharpness);
        }
        anyhow::ensure!(!layers.is_empty(), "material set has no usable textures");
        let side = alphas.size()[0].clamp(1, CONTROL_MAX);
        let planes = Arc::new(SplatPlanes {
            layer_of: self.layer_of(),
            side,
            maps: Mutex::new((ids.clone(), alphas.clone())),
            drawn: Mutex::new(None),
            filled: Mutex::new(None),
        });
        let [i0, w0, i1, w1] = std::array::from_fn(|k| {
            let planes = planes.clone();
            ControlMap::drawn(side, side, move |rect| planes.plane(k, rect)).map(Arc::new)
        });
        // Maps the splat leaves out never upload, so nothing would let
        // their texels go: 31 MB of normals on the continental island's set.
        // They read again should a later splat take them.
        let unused = |maps: &mut Vec<Image>| {
            for map in maps.drain(..) {
                if let Image::Releasable(r) = map {
                    r.release();
                }
            }
        };
        // Normals light every layer or none: the kit takes one per layer.
        if normals.len() != layers.len() {
            if !normals.is_empty() {
                eprintln!(
                    "material set: {} of {} layers ship normal maps; ground drawn without",
                    normals.len(),
                    layers.len()
                );
            }
            unused(&mut normals);
        }
        // Gloss too: matte ground beats some layers shining.
        if gloss.len() != layers.len() {
            unused(&mut gloss);
        }
        // Relief needs every layer's height: a layer without would flatten
        // the ledges its neighbours raise. Drawn when the view asks
        // (`Relief::on`).
        // The kit stacks them in one array from the smallest's size down, so
        // each must have that size among its mips. Wetlands 01's set ships
        // one 2048 height map without mips beside 1024s; it panicked the
        // splat's validation, so such a set draws without relief.
        let smallest =
            (heights.iter().map(|h| h.size())).min_by_key(|&(w, h)| u64::from(w) * u64::from(h));
        let shared = heights.iter().all(|h| {
            let (w, ht) = h.size();
            (0..h.levels()).any(|l| Some(((w >> l).max(1), (ht >> l).max(1))) == smallest)
        });
        if heights.len() == layers.len() && !shared {
            eprintln!(
                "material set: its height maps share no mip size; ground drawn without relief"
            );
        }
        if heights.len() != layers.len() || !shared {
            unused(&mut heights);
        }
        let relief = (!heights.is_empty()).then(|| Relief {
            heights,
            ranges,
            on: false,
        });
        let grit = match (&self.grit, grit) {
            (Some(g), Some(layer)) => Some(Grit {
                texture: g.texture.clone(),
                mask: Arc::new(grit_mask(layer)?),
                metres: layer.width as f32 * layer.cell_metres,
                tiling: g.tiling,
                sharpness: g.sharpness,
                slope: g.slope,
            }),
            _ => None,
        };
        let splat = Splat {
            layers,
            tiling,
            sharpness,
            normals,
            gloss,
            indices: vec![i0?, i1?],
            weights: vec![w0?, w1?],
            grit,
            bake,
            relief,
        };
        // Game files: a set whose layers can't share an array fails here,
        // not in the renderer.
        splat.validate()?;
        Ok((splat, planes))
    }

    /// Hands `splat`'s planes the material maps as they are now and marks
    /// its control texels under `area` (island metres x0, z0, x1, z1)
    /// changed, after a paint dab. The GPU picks the change up on its next
    /// frame.
    pub fn update_splat<M: MaterialWords + Clone + Send + 'static>(
        &self,
        splat: &Splat,
        planes: &SplatPlanes<M>,
        ids: &M,
        alphas: &M,
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
        planes.set(ids, alphas);
        // The dab's planes now, rounded out to the 4x4 blocks compressed
        // uploads write. Made in the frame after it, they cost that frame
        // 1-1.5 ms more CPU on the pirate island than the held planes did.
        let block = |v: usize| (v / 4 * 4, v.next_multiple_of(4).min(side));
        let filled = Rect {
            x0: block(rect.x0).0,
            z0: block(rect.z0).0,
            x1: block(rect.x1).1,
            z1: block(rect.z1).1,
        };
        if (filled.x1 - filled.x0) * (filled.z1 - filled.z0) <= FILL_MAX {
            planes.fill(filled);
        }
        for map in splat.indices.iter().chain(&splat.weights) {
            map.touch(rect);
        }
    }

    /// Material id to layer (`stack` order), `NO_LAYER` where the set has
    /// none.
    fn layer_of(&self) -> [u8; 256] {
        let mut layer_of = [NO_LAYER; 256];
        for (i, &id) in self.stack.iter().enumerate() {
            layer_of[usize::from(id)] = i as u8;
        }
        layer_of
    }
}

/// A material id no layer of the set draws.
const NO_LAYER: u8 = u8::MAX;

/// Control texels an edit makes its planes for at once (`SplatPlanes::fill`),
/// 4 MB of them: past that, as for an undo of a stroke across the island,
/// the reads make them.
const FILL_MAX: usize = 512 * 512;

/// What a splat's control maps draw ([`ControlMap::drawn`]): its four
/// control planes, computed from the material maps for the rectangle read.
/// Held, at 2048² they were 64 MB beside the maps they came from.
pub struct SplatPlanes<M> {
    layer_of: [u8; 256],
    side: usize,
    /// Shared with the document (copy-on-write chunks): a clone is cheap.
    maps: Mutex<(M, M)>,
    /// The last rectangle computed and its planes not yet read: the kit
    /// reads all four over one rectangle in turn, and one pass makes them.
    drawn: Mutex<Option<(Rect, [Option<Vec<u8>>; 4])>>,
    /// The last edit's rectangle and its planes (`fill`): the uploads and
    /// relief scans of the frame after it read within it.
    filled: Mutex<Option<(Rect, [Vec<u8>; 4])>>,
}

impl<M: MaterialWords + Clone + Send> SplatPlanes<M> {
    /// The material maps to draw from from now on.
    pub fn set(&self, ids: &M, alphas: &M) {
        *self.maps.lock().unwrap() = (ids.clone(), alphas.clone());
        *self.drawn.lock().unwrap() = None;
        *self.filled.lock().unwrap() = None;
    }

    /// Makes the planes of `rect` now, for reads within it until the next
    /// `set`.
    fn fill(&self, rect: Rect) {
        let (ids, alphas) = self.maps.lock().unwrap().clone();
        let planes = Palette::control_planes(&self.layer_of, &ids, &alphas, self.side, rect);
        *self.filled.lock().unwrap() = Some((rect, planes));
    }

    /// Plane `k` (index A, weight A, index B, weight B) of `rect`.
    fn plane(&self, k: usize, rect: Rect) -> Vec<u8> {
        if let Some((at, planes)) = self.filled.lock().unwrap().as_ref()
            && (at.x0 <= rect.x0 && at.z0 <= rect.z0)
            && (rect.x1 <= at.x1 && rect.z1 <= at.z1)
        {
            let (width, stride) = (rect.x1 - rect.x0, at.x1 - at.x0);
            let mut out = Vec::with_capacity(width * (rect.z1 - rect.z0) * 4);
            for z in rect.z0..rect.z1 {
                let row = ((z - at.z0) * stride + rect.x0 - at.x0) * 4;
                out.extend_from_slice(&planes[k][row..row + width * 4]);
            }
            return out;
        }
        let mut drawn = self.drawn.lock().unwrap();
        if let Some((at, planes)) = drawn.as_mut()
            && *at == rect
            && let Some(plane) = planes[k].take()
        {
            if planes.iter().all(Option::is_none) {
                *drawn = None;
            }
            return plane;
        }
        let (ids, alphas) = self.maps.lock().unwrap().clone();
        let mut planes =
            Palette::control_planes(&self.layer_of, &ids, &alphas, self.side, rect).map(Some);
        let plane = planes[k].take().expect("just made");
        *drawn = Some((rect, planes));
        plane
    }
}

impl Palette {
    /// The four control planes (index A, weight A, index B, weight B; RGBA
    /// rows) of control texels `rect` on a `side`² map. `layer_of` maps
    /// material ids to layers in `stack` order, as in [`Palette::splat`],
    /// which is the order the kit lays them over each other: six lookups per
    /// texel, millions of texels.
    fn control_planes(
        layer_of: &[u8; 256],
        ids: &(impl MaterialWords + ?Sized),
        alphas: &(impl MaterialWords + ?Sized),
        side: usize,
        rect: Rect,
    ) -> [Vec<u8>; 4] {
        let ([iw, ih], [aw, ah]) = (ids.size(), alphas.size());
        let per_texel = aw as f32 / side as f32;
        let (width, rows) = (rect.x1 - rect.x0, rect.z1 - rect.z0);
        // Index A, weight A, index B, weight B: RGBA rows each, filled in
        // bands of rows, on as many cores as there are 16k texels: palettes
        // and relief scans read small rectangles every frame of a stroke,
        // where a thread per core cost more than the texels.
        let mut planes: [Vec<u8>; 4] = std::array::from_fn(|_| vec![0u8; width * rows * 4]);
        let cores = std::thread::available_parallelism().map_or(4, |n| n.get());
        let threads = (width * rows).div_ceil(16 << 10).clamp(1, cores);
        let band = rows.div_ceil(threads).max(1) * width * 4;
        // Each output row reads one row of alphas and of ids, fetched once
        // as runs: a word at a time through the chunks cost 30 ns a texel.
        let ax = |x: usize| (((x as f32 + 0.5) * per_texel) as usize).min(aw - 1);
        let (ax0, ax1) = (ax(rect.x0), ax(rect.x1.max(rect.x0 + 1) - 1) + 1);
        let (ix0, ix1) = ((ax0 / 2).min(iw - 1), ((ax1 - 1) / 2).min(iw - 1) + 1);
        let fill = |n: usize, ia: &mut [u8], wa: &mut [u8], ib: &mut [u8], wb: &mut [u8]| {
            let z0 = rect.z0 + n * band / (width * 4);
            let (mut alpha_row, mut id_row) = (Vec::new(), Vec::new());
            for (r, y) in (z0..z0 + ia.len() / (width * 4)).enumerate() {
                let ay = (((y as f32 + 0.5) * per_texel) as usize).min(ah - 1);
                alpha_row.clear();
                id_row.clear();
                alphas.row_into(ay, ax0..ax1, &mut alpha_row);
                ids.row_into((ay / 2).min(ih - 1), ix0..ix1, &mut id_row);
                for x in 0..width {
                    let a = ax(rect.x0 + x);
                    let slots = island::unpack_slots(id_row[(a / 2).min(iw - 1) - ix0]);
                    let weights = island::unpack_slots(alpha_row[a - ax0]);
                    let t = r * width + x;
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
            }
        };
        let [ia, wa, ib, wb] = &mut planes;
        if threads == 1 {
            fill(0, ia, wa, ib, wb);
        } else {
            std::thread::scope(|scope| {
                let bands = (ia.chunks_mut(band).zip(wa.chunks_mut(band)))
                    .zip(ib.chunks_mut(band).zip(wb.chunks_mut(band)));
                for (n, ((ia, wa), (ib, wb))) in bands.enumerate() {
                    let fill = &fill;
                    scope.spawn(move || fill(n, ia, wa, ib, wb));
                }
            });
        }
        planes
    }

    /// Bakes ground color at `BAKE_PER_METRE` over `size` metres from the
    /// level-0 material maps (`ids`: one word per 2×2 alpha texels).
    pub fn bake(
        &self,
        ids: &impl MaterialWords,
        alphas: &impl MaterialWords,
        size: f32,
    ) -> Result<Texture> {
        let side = (size as usize * BAKE_PER_METRE).clamp(1, BAKE_MAX);
        let per_metre = side as f32 / size;
        let ([iw, ih], [aw, ah]) = (ids.size(), alphas.size());
        let alpha_per_texel = aw as f32 / side as f32;
        let mut rgba = vec![0u8; side * side * 4];
        par_rows(&mut rgba, side * 4, |y, row| {
            for (x, out) in row.chunks_mut(4).enumerate() {
                let ax = (((x as f32 + 0.5) * alpha_per_texel) as usize).min(aw - 1);
                let ay = (((y as f32 + 0.5) * alpha_per_texel) as usize).min(ah - 1);
                let slots =
                    island::unpack_slots(ids.word((ax / 2).min(iw - 1), (ay / 2).min(ih - 1)));
                let shown = self.visible(slots, island::unpack_slots(alphas.word(ax, ay)));
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

impl Palette {
    /// How far the ground sinks at each sample of `field` where the set
    /// lays a sinking material (see `sink`), as the game's coarse
    /// displacement moves it: each material's coverage interpolated over
    /// the four nearest texels and stacked as the bake stacks it. `None`
    /// when the set sinks nothing. The game displaces every material by its
    /// height texture within its range; only marsh sinks far enough to
    /// matter here (its floor dips under the marsh lake, which is how its
    /// pools of standing water show), so the rest keep their heights and
    /// sinking ground takes its range's middle, without the texture.
    // TODO(alex): materials added from another set (add_material) sink 0.
    pub fn sinking(
        &self,
        ids: &impl MaterialWords,
        alphas: &impl MaterialWords,
        field: gpui_editor_kit::heightfield::Grid,
    ) -> Option<Vec<f32>> {
        use rayon::prelude::*;
        let ([iw, ih], [aw, ah]) = (ids.size(), alphas.size());
        if self.sink.iter().all(|&s| s == 0.) || aw == 0 || iw == 0 {
            return None;
        }
        let per_metre = aw as f32 / field.size()[0].max(1e-3);
        let mut out = vec![0f32; field.width * field.depth];
        out.par_chunks_mut(field.width)
            .enumerate()
            .for_each(|(z, row)| {
                let mut cover: Vec<(u8, f32)> = Vec::with_capacity(24);
                for (x, out) in row.iter_mut().enumerate() {
                    let (u, v) = (
                        (field.origin[0] + x as f32 * field.spacing) * per_metre - 0.5,
                        (field.origin[1] + z as f32 * field.spacing) * per_metre - 0.5,
                    );
                    let (fx, fy) = (u - u.floor(), v - v.floor());
                    cover.clear();
                    for t in 0..4 {
                        let (dx, dy) = (t & 1, t >> 1);
                        let tx = (u.floor() as i64 + dx).clamp(0, aw as i64 - 1) as usize;
                        let ty = (v.floor() as i64 + dy).clamp(0, ah as i64 - 1) as usize;
                        let share =
                            if dx == 1 { fx } else { 1. - fx } * if dy == 1 { fy } else { 1. - fy };
                        let slots = island::unpack_slots(
                            ids.word((tx / 2).min(iw - 1), (ty / 2).min(ih - 1)),
                        );
                        let weights = island::unpack_slots(alphas.word(tx, ty));
                        for k in (0..6).filter(|&k| weights[k] > 0) {
                            cover.push((slots[k] & 31, f32::from(weights[k]) / 31. * share));
                        }
                    }
                    let n = self.shares(&mut cover);
                    *out = cover[..n]
                        .iter()
                        .map(|&(id, s)| s * self.sink[usize::from(id)])
                        .sum();
                }
            });
        // The game samples coverage bicubically for displacement (C1 at
        // material borders); bilinear coverage of near-binary marsh texels
        // leaves stair-stepped pool edges. A box of half a metre each way
        // rounds them; wider ones fill the narrow pools in.
        let radius = ((0.5 / field.spacing).round() as usize).max(1);
        let (w, d) = (field.width, field.depth);
        {
            let mut rows = vec![0f32; w * d];
            rows.par_chunks_mut(w).enumerate().for_each(|(z, row)| {
                for (x, v) in row.iter_mut().enumerate() {
                    let (a, b) = (x.saturating_sub(radius), (x + radius).min(w - 1));
                    *v = out[z * w + a..=z * w + b].iter().sum::<f32>() / (b - a + 1) as f32;
                }
            });
            out.par_chunks_mut(w).enumerate().for_each(|(z, row)| {
                let (a, b) = (z.saturating_sub(radius), (z + radius).min(d - 1));
                for (x, v) in row.iter_mut().enumerate() {
                    *v = (a..=b).map(|k| rows[k * w + x]).sum::<f32>() / (b - a + 1) as f32;
                }
            });
        }
        Some(out)
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
                additive: false,
                unlit: false,
            }),
            ..Default::default()
        })
        .expect("water quad"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wall_test_uses_face_normals() {
        let flat = normal([[0., 0., 0.], [0., 0., 1.], [1., 0., 0.]]);
        let wall = normal([[0., 0., 0.], [1., 0., 0.], [0., 1., 0.]]);
        assert!(flat[1].abs() > 0.99 && wall[1].abs() < 0.01);
    }

    // Needs the game: the celtic set sinks its marsh floor half a metre
    // (the middle of -0.57..-0.505) under ground covered by it alone, not
    // where grass lies over it; the roman set sinks nothing.
    #[test]
    fn marsh_floor_sinks_the_ground_as_its_displacement_does() {
        use anno_formats::a7m::island::{Grid, pack_slots};
        let Some(dir) = anno_formats::vfs::install_dir("ANNO117_DIR") else {
            eprintln!("skipping: ANNO117_DIR unset");
            return;
        };
        let families = crate::games::profile(anno_core::game::Game::Anno117).archive_families;
        let install = anno_formats::vfs::Install::open(dir, families).unwrap();
        let sets = "data/base/graphics/library/environment/material_sets";
        let celtic =
            super::Palette::load(&install, None, &format!("{sets}/celtic_materialset_01.xml"))
                .unwrap();
        let roman =
            super::Palette::load(&install, None, &format!("{sets}/roman_materialset_01.xml"))
                .unwrap();
        let id = |p: &super::Palette, name: &str| {
            *p.materials
                .iter()
                .find(|(_, m)| m.file.contains(name))
                .unwrap()
                .0
        };
        let (marsh, grass) = (id(&celtic, "marshfloor"), id(&celtic, "grass"));
        // 32 m: marsh floor everywhere, grass laid fully over the east half.
        let side = 64;
        let ids = Grid {
            width: side / 2,
            height: side / 2,
            data: vec![pack_slots([marsh, grass, 0, 0, 0, 0]); side * side / 4],
        };
        let alphas = Grid {
            width: side,
            height: side,
            data: (0..side * side)
                .map(|i| pack_slots([31, if i % side >= side / 2 { 31 } else { 0 }, 0, 0, 0, 0]))
                .collect(),
        };
        let field = gpui_editor_kit::heightfield::Grid {
            width: 65,
            depth: 65,
            spacing: 0.5,
            origin: [0.; 2],
        };
        let sink = celtic
            .sinking(&ids, &alphas, field)
            .expect("the celtic set sinks marsh");
        let at = |x: usize| sink[32 * 65 + x];
        assert!((at(8) + 0.5375).abs() < 0.01, "marsh {}", at(8));
        assert!(at(56).abs() < 0.01, "grass over it {}", at(56));
        assert!(roman.sinking(&ids, &alphas, field).is_none());
    }

    // Needs the game: each of the Roman set's maps read from its kept
    // levels is the file read whole and shrunk, level for level.
    #[test]
    fn ground_maps_read_only_the_levels_they_keep() {
        use gpui_editor_kit::texture::Dds;
        let Some(dir) = anno_formats::vfs::install_dir("ANNO117_DIR") else {
            eprintln!("skipping: ANNO117_DIR unset");
            return;
        };
        let families = crate::games::profile(anno_core::game::Game::Anno117).archive_families;
        let install = anno_formats::vfs::Install::open(dir, families).unwrap();
        let set = "data/base/graphics/library/environment/material_sets/roman_materialset_01.xml";
        let xml = String::from_utf8(install.read(set).unwrap()).unwrap();
        let mut checked = 0;
        for block in xml.split("<Material>").skip(1) {
            let Some(file) = super::tag(block, "FileName").filter(|f| !f.is_empty()) else {
                continue;
            };
            let material = String::from_utf8_lossy(&install.read(file).unwrap()).into_owned();
            for name in [
                "DiffuseFileName",
                "NormalFileName",
                "SpecFileName",
                "HeightFileName",
            ] {
                let Some(path) = super::tag(&material, name).map(super::shipped) else {
                    continue;
                };
                let bytes = install.read(&path).unwrap();
                let mut whole = Dds::parse(&bytes).unwrap().compressed(&bytes).unwrap();
                whole.shrink(super::MATERIAL_MIP);
                let kept = super::capped(&install, &path, super::MATERIAL_MIP)
                    .unwrap()
                    .1
                    .unwrap();
                assert_eq!(
                    (kept.block, kept.width, kept.height, &kept.mips),
                    (whole.block, whole.width, whole.height, &whole.mips),
                    "{path}"
                );
                checked += 1;
            }
        }
        assert!(checked > 30, "{checked} maps");
    }

    // Needs the game: the Roman set ships a gloss map with every material
    // and grit in its header, and a splat over a grit map carries both.
    #[test]
    fn roman_set_gives_the_splat_gloss_and_grit() {
        let Some(dir) = anno_formats::vfs::install_dir("ANNO117_DIR") else {
            eprintln!("skipping: ANNO117_DIR unset");
            return;
        };
        let families = crate::games::profile(anno_core::game::Game::Anno117).archive_families;
        let install = anno_formats::vfs::Install::open(dir, families).unwrap();
        let set = "data/base/graphics/library/environment/material_sets/roman_materialset_01.xml";
        let palette = super::Palette::load(&install, None, set).unwrap();
        assert!(palette.materials.values().all(|m| m.gloss.is_some()));
        let grit = palette.grit.as_ref().expect("the set's grit texture");
        assert_eq!((grit.tiling, grit.sharpness, grit.slope), (4., 5., 0.2));
        let path = install
            .search("roman_island_large_01.a7m", 10)
            .into_iter()
            .map(|id| install.file_path(id).to_owned())
            .find(|p| p.ends_with(".a7m"))
            .unwrap();
        let archive =
            anno_formats::rda::Archive::parse(install.read(&path).unwrap(), &path).unwrap();
        let island = anno_formats::a7m::island::Island::parse(
            &archive.read_path("gamedata.data").unwrap(),
            &archive.read_path("rd3d.data").unwrap(),
        )
        .unwrap();
        let grids = anno_formats::a7m::grids::Grids::read(&island).unwrap();
        let layer = grids.layer(anno_formats::a7m::grids::Kind::Grit).unwrap();
        let grid = |v: u32| anno_formats::a7m::island::Grid {
            width: 4,
            height: 4,
            data: vec![v; 16],
        };
        let (ids, alphas) = (grid(u32::from(palette.stack[0])), grid(31));
        let (splat, _) = palette
            .splat(
                crate::games::a7m_rules(anno_core::game::Game::Anno117).ground_bake,
                &ids,
                &alphas,
                Some(layer),
            )
            .unwrap();
        assert_eq!(splat.gloss.len(), splat.layers.len());
        let g = splat.grit.unwrap();
        assert_eq!(g.metres, layer.width as f32 * layer.cell_metres);
        assert_eq!(g.mask.size(), [layer.width, layer.height]);
    }
}
