// A 2070 island (`.isd`) as the editor's core island. Heights at their 1 m
// corners, paint in its own `Layers` form and the two authored blocker
// grids come into the document through the `.isd` codec; they go
// back through its writers into a fresh parse of the file as loaded: the codec's rules (gameplay
// heights, `Flags`, the submarine blocker) apply, and everything the document
// doesn't hold is carried byte for byte. Blocking lifted ground is the
// editor's: it blocks as the heights change, so the path blockers save as
// the document shows them. Trees,
// rocks and grass come in as the document's props and buildings, walls,
// units and decoration as its objects, and their edits go back into their
// records (`objects`).
// Engine addresses here are Anno5.exe build 11514956 (Steam).

use std::ops::RangeInclusive;
use std::sync::Arc;

use anno_core::island::{Blockers, Doc, Island, Layers, Materials, Paint, Registry};
use anno_formats::isd::grids::{BuildBlockers, PathBlockers, PathLayer, Sculpt};
use anno_formats::isd::terrain::{GroundTextures, Terrain};
use anno_formats::vfs::Install;
use anyhow::{Context, Result, ensure};
use gpui_editor_kit::heightfield::{Grid, Heightfield};

/// Gameplay heights of buildable land, in 1/1024 m: the 0.7 m plateau.
/// The game derives buildability from them at load (0x140B9D780: `4h −
/// 2703 ≤ 369`, unsigned); both edges rest on that reading.
pub const BUILDABLE: RangeInclusive<i16> = 676..=768;

/// Gameplay heights of buildable ground under water, in 1/1024 m: the Deep
/// Ocean plateau, -27.10 to -26.90 m. The same test right after the land
/// band's (0x140B9DF6D: `4h + 111001 ≤ 819`, unsigned) clears the cell, on
/// every island, so either band builds.
pub const UNDERWATER_BUILDABLE: RangeInclusive<i16> = -27_750..=-27_546;

/// Whether a cell of gameplay height `h` (`isd::grids::gameplay_height`)
/// is buildable by its height.
pub(crate) fn buildable(h: i16) -> bool {
    BUILDABLE.contains(&h) || UNDERWATER_BUILDABLE.contains(&h)
}

/// Where a first sculpt or dab puts the corners of an empty ocean chunk it
/// gives heights, unless its neighbours already hold them: the depth the
/// shipped ocean floor sits at around empty chunks (the median corner on
/// their rims is -39.7 m, and a tenth lie at -40 m or deeper).
const SEABED: f32 = -40.;

/// What a sculpt fills in on a 2070 island. Lifts past the land's band
/// block: shipped islands block 97% of their ground above it and leave 98%
/// of the band open.
pub const SCULPT: Sculpt = Sculpt {
    block_lifted_above: Some(*BUILDABLE.end()),
    seabed: SEABED,
};

/// The ground texture registry every island's paint layers name.
pub const REGISTRY: &str = "data/config/engine/groundtextures.xml";

/// `install`'s ground texture registry, as the paint and ground read it.
pub fn registry(install: &Install) -> Result<Arc<Registry>> {
    let xml = install.read(REGISTRY)?;
    let xml = String::from_utf8_lossy(&xml);
    let textures = GroundTextures::parse(&xml).with_context(|| REGISTRY.to_owned())?;
    let scale = textures.scale();
    Ok(Arc::new(Registry::new(textures, scale)?))
}

/// Island `path` from `install`, its objects drawn with the models
/// `assets`, the install's database, names. Without it (the editor's is
/// still loading, or a tool has none) the open reads the database itself.
/// The first open starts indexing the install's shipped records, which
/// Place and conversion copy (`objects::Shipped`).
pub fn read(
    install: &Install,
    path: &str,
    assets: Option<&anno_formats::assets::Assets>,
) -> Result<Island> {
    crate::objects::Shipped::warm(install);
    let read;
    let assets = match assets {
        Some(assets) => Some(assets),
        None => {
            read = crate::assets::load(install)
                .map_err(|e| eprintln!("{path}: objects drawn without models: {e:#}"))
                .ok();
            read.as_ref()
        }
    };
    from_bytes(install.read(path)?.into(), path, registry(install)?, assets)
}

/// The island in `bytes`; `file` names it in errors. Its trees, rocks and
/// grass become props whose models `assets` names (without it every one is
/// a marker), and its other records the document's objects.
pub fn from_bytes(
    bytes: Arc<[u8]>,
    file: &str,
    registry: Arc<Registry>,
    assets: Option<&anno_formats::assets::Assets>,
) -> Result<Island> {
    let doc = anno_formats::isd::Doc::parse(&bytes).with_context(|| file.to_owned())?;
    let records = anno_formats::isd::objects::read(&doc, file)?;
    let (meshes, props) = crate::objects::props(&records, assets);
    let objects = crate::objects::document_objects(&records);
    let terrain = Terrain::read(&doc, file)?;
    let corners = terrain.corners();
    let (width, depth) = corners.size();
    // Corners only empty ocean touches hold no height. They show at the
    // depth a first sculpt or dab would give them, and a save that finds
    // them still there leaves their chunks empty.
    let heights = (0..depth)
        .flat_map(|z| (0..width).map(move |x| (x, z)))
        .map(|(x, z)| corners.get(x, z).unwrap_or(SEABED))
        .collect();
    let grid = Grid {
        width,
        depth,
        spacing: 1.,
        origin: [0.; 2],
    };
    let field = Arc::new(Heightfield::new(grid, heights));
    let shapes = match doc.root("BuildBlockerShapes") {
        Some(_) => Some(BuildBlockers::read(&doc, file)?),
        None => None,
    };
    let mut blockers = Blockers::new(&PathBlockers::read(&doc, file)?, shapes);
    // Shipped Deep Ocean islands block 94% of their ground above the
    // plateau's band and leave 84% of the band open
    // (`path_blockers_against_the_buildable_bands`), as surface islands do
    // with the land's: lifts past it block too.
    blockers.under_water = crate::project::lies_under_water(&doc);
    let paint = Paint::Layers(Layers::read(&terrain, registry)?.with_blockers(blockers));
    let doc = Doc {
        land_heights: field.clone(),
        field,
        // Paint sits on the metre vertices.
        materials: Some(Materials::from_paint(paint, 1.)),
        meshes: Arc::new(meshes),
        props: props.into(),
        objects: Arc::new(objects),
        grids: None,
        tint: None,
        session: Default::default(),
        heights_patch: None,
    };
    let mut island = Island::new(doc, None, String::new());
    island.set_native(anno_core::island::Native::File(bytes));
    Ok(island)
}

/// The bytes of `island`, which `read` or `from_bytes` gave, with its
/// edits: objects and props, heights, then paint, then the blockers, each
/// through the codec's writer. `assets` names the records of trees, rocks
/// and grass of a kind the file never placed (a conversion's); `file`
/// names it in errors.
pub fn write(
    island: &Island,
    file: &str,
    assets: Option<&anno_formats::assets::Assets>,
) -> Result<Vec<u8>> {
    let loaded = (island.native.as_ref().and_then(|n| n.file()))
        .context("not a 2070 island read from its file")?;
    let mut out = anno_formats::isd::Doc::parse(loaded)?;
    let doc = &island.doc;
    crate::objects::write(
        &mut out,
        &doc.props,
        &doc.meshes,
        &doc.objects,
        &island.foreign,
        assets,
        file,
    )?;
    // The movement grids as loaded: the cells the document changed from
    // these are its edits.
    let loaded_paths = PathBlockers::read(&out, file)?;

    let terrain = Terrain::read(&out, file)?;
    let mut corners = terrain.corners();
    let (width, depth) = corners.size();
    ensure!(
        (doc.field.grid.width, doc.field.grid.depth) == (width, depth),
        "{file}: heights of {}×{} for {width}×{depth} corners",
        doc.field.grid.width,
        doc.field.grid.depth
    );
    // Untouched corners hold their loaded bits, and so write nothing.
    for z in 0..depth {
        for x in 0..width {
            let height = doc.field.heights[z * width + x];
            let moved = match corners.get(x, z) {
                Some(was) => was.to_bits() != height.to_bits(),
                None => height.to_bits() != SEABED.to_bits(),
            };
            if moved {
                corners.set(x, z, height);
            }
        }
    }
    drop(terrain);
    // A document with blockers already blocked what its sculpts lifted, and
    // a Grid stroke may have cleared some of those cells since: the codec
    // must not block them again from the file's heights.
    let sculpt = Sculpt {
        block_lifted_above: SCULPT
            .block_lifted_above
            .filter(|_| doc.blockers().is_none()),
        ..SCULPT
    };
    corners.write(&mut out, file, sculpt)?;

    if let Some(m) = &doc.materials {
        let Paint::Layers(paint) = &m.paint else {
            anyhow::bail!("{file}: a 2070 island holds 2070 paint, not a7m");
        };
        write_paint(&mut out, file, paint)?;
    }

    if let Some(b) = doc.blockers() {
        // The document's cells, written where they differ from the file.
        let mut paths = PathBlockers::read(&out, file)?;
        let (cols, rows) = paths.size();
        ensure!(
            b.size() == (cols, rows),
            "{file}: movement grids of {:?} for {cols}×{rows} cells",
            b.size()
        );
        for layer in [PathLayer::Blocked, PathLayer::HighGround] {
            for z in 0..rows {
                for x in 0..cols {
                    let on = b.get(layer, x, z);
                    if on != loaded_paths.get(layer, x, z) {
                        paths.set(layer, x, z, on);
                    }
                }
            }
        }
        paths.write(&mut out, file)?;
        if let Some(shapes) = &b.shapes {
            shapes.write(&mut out, file)?;
        }
    }
    Ok(out.encode())
}

/// Writes `paint` into `out` through the codec's writer: each painted
/// chunk whose layers differ from the file's. An unpainted chunk is left as
/// the file has it: a first sculpt may have given it paint, as the codec
/// does.
pub fn write_paint(out: &mut anno_formats::isd::Doc, file: &str, paint: &Layers) -> Result<()> {
    let terrain = Terrain::read(out, file)?;
    let mut layers = terrain.layers();
    let (w, h) = terrain.size();
    ensure!(
        paint.chunks() == [w, h],
        "{file}: paint for {:?} chunks on an island of {w}×{h}",
        paint.chunks()
    );
    for k in 0..w * h {
        let ids = paint.ids(k);
        if ids.is_empty() {
            continue;
        }
        let same = (terrain.ids(k).iter().copied()).eq(ids.iter().map(|&id| u32::from(id)))
            && (0..ids.len()).all(|l| terrain.plane(k, l) == paint.plane(k, l));
        if !same {
            let planes: Vec<u8> = (0..ids.len()).flat_map(|l| *paint.plane(k, l)).collect();
            let ids: Vec<u32> = ids.iter().map(|&id| u32::from(id)).collect();
            layers.set(k, &ids, &planes)?;
        }
    }
    drop(terrain);
    layers.write(out, file, SCULPT)
}

/// 2070's ground for one island: its layer textures, the paint the mounted
/// ground follows (`LivePaint`), and the ground draw.
pub struct Ground {
    textures: Arc<anno_render::layers::LayerTextures>,
    live: Arc<anno_render::layers::LivePaint>,
    draw: gpui_editor_kit::gpu::GroundDraw,
    registry: Arc<Registry>,
}

impl Ground {
    /// The ground for `paint`, its layers read from `install`.
    pub fn new(install: &Install, paint: &Layers) -> Result<Self> {
        use anno_render::layers::{LayerPaint, LayerTextures, LivePaint, draw_live};
        // 512 texels a side: 40 layers' diffuse and normal maps in about
        // 20 MB.
        let textures = Arc::new(LayerTextures::of(install, paint.registry(), 512)?);
        let live = LivePaint::new(Arc::new(LayerPaint::of(paint, &textures)));
        let draw = draw_live(live.clone(), textures.clone(), [0.; 2]);
        Ok(Self {
            textures,
            live,
            draw,
            registry: paint.registry().clone(),
        })
    }
}

impl anno_core::game::IslandGround for Ground {
    fn draw(&self) -> gpui_editor_kit::gpu::GroundDraw {
        self.draw.clone()
    }

    fn update(&self, paint: &Paint, area: [f32; 4]) {
        if let Paint::Layers(paint) = paint {
            self.live.update(paint, &self.textures, area);
        }
    }

    // From the smallest mip of at least 64 texels of each layer's diffuse
    // texture, or for a scatter layer (grass, bushes, kelp) the texture its
    // objects wear. Layers that fail to read are left out.
    fn thumbs(&self, install: &Install) -> Vec<anno_core::game::Thumb> {
        use gpui_editor_kit::texture::Dds;
        let mut out = Vec::new();
        for t in self.registry.textures().textures() {
            let Ok(id) = u8::try_from(t.id) else {
                continue;
            };
            let Some(source) = t.diffuse.as_ref().or(t.grass.as_ref()) else {
                continue;
            };
            let colour = t.diffuse.is_some();
            let path = anno_formats::model::texture_path(source);
            let image = (|| -> Result<_> {
                let bytes = install.read(&path)?;
                let dds = Dds::parse(&bytes)?;
                let mut texture = dds.decode(&bytes, dds.mip_for(64))?;
                // Diffuse alpha is height and gloss, not coverage; a grass
                // texture's is the blades' cut-out, which the thumbnail
                // keeps.
                if colour {
                    texture
                        .rgba
                        .iter_mut()
                        .skip(3)
                        .step_by(4)
                        .for_each(|a| *a = 255);
                }
                Ok(texture)
            })();
            match image {
                Ok(image) => out.push(anno_core::game::Thumb {
                    id,
                    name: source.replace('\\', "/"),
                    image,
                    colour,
                }),
                Err(e) => eprintln!("{path}: no thumbnail: {e:#}"),
            }
        }
        out
    }

    fn stand_in(&self, id: u8) -> bool {
        self.registry.textures().placeholder(u32::from(id))
    }

    // Layers that use the developers' magenta stand-in texture
    // (`GroundTextures::placeholder`), and how many 1 m cells show them. The
    // game draws them at its default preset, so the editor draws them too
    // and only says so.
    fn note(&self, paint: &Paint) -> Option<String> {
        use anno_formats::isd::terrain::Terrain;
        let Paint::Layers(paint) = paint else {
            return None;
        };
        let registry = paint.registry().textures();
        let [w, h] = paint.chunks();
        // Per id, the cells it covers. A plane's last row and column are
        // the next chunk's first, so each chunk counts its own 16 × 16.
        let mut cells = [0usize; 256];
        for k in 0..w * h {
            for (l, &id) in paint.ids(k).iter().enumerate() {
                if !registry.placeholder(u32::from(id)) {
                    continue;
                }
                let plane = paint.plane(k, l);
                cells[usize::from(id)] += (0..Terrain::CHUNK * Terrain::CHUNK)
                    .filter(|n| {
                        plane[n / Terrain::CHUNK * Terrain::PLANE_SIDE + n % Terrain::CHUNK] > 0
                    })
                    .count();
            }
        }
        let used: Vec<String> = (cells.iter().enumerate())
            .filter(|&(_, &n)| n > 0)
            .map(|(id, &n)| {
                let name = (registry.textures().iter())
                    .find(|t| t.id == id as u32)
                    .map_or("", |t| t.name.as_str());
                format!("{id} {name} ({n} m²)")
            })
            .collect();
        (!used.is_empty()).then(|| {
            format!(
                "Placeholder ground layers draw magenta in game: {}",
                used.join(", ")
            )
        })
    }
}

/// Island `path` from above, `edge` texels a side, from its terrain
/// heights: the files ship no map picture. Heights count from the island's
/// own `SeaLevel`, so an underwater island's plateau shows as its land.
pub(crate) fn top_view(
    install: &Install,
    path: &str,
    edge: u32,
) -> Option<gpui_editor_kit::texture::Texture> {
    // Metres over the sea level: land above 0, shallows down to -10, the
    // rest clear. 60 m is about the tallest shipped land, where the colour
    // stops changing.
    const SHALLOW: f32 = -10.;
    const TALL: f32 = 60.;
    const SHORE: [f32; 3] = [104., 142., 76.];
    const PEAK: [f32; 3] = [156., 146., 126.];
    const SHALLOWS: [u8; 4] = [120, 190, 210, 140];
    let bytes = install.read(path).ok()?;
    let doc = anno_formats::isd::Doc::parse(&bytes).ok()?;
    let sea = crate::project::sea_level(&doc) as f32;
    let corners = Terrain::read(&doc, path).ok()?.corners();
    let (w, d) = corners.size();
    let edge = edge.max(1);
    let side = w.max(d) as f32;
    let mut rgba = vec![0u8; edge as usize * edge as usize * 4];
    for (n, texel) in rgba.as_chunks_mut::<4>().0.iter_mut().enumerate() {
        let at = |p: usize| ((p as f32 + 0.5) / edge as f32 * side) as usize;
        let (x, z) = (at(n % edge as usize), at(n / edge as usize));
        let Some(h) = (x < w && z < d).then(|| corners.get(x, z)).flatten() else {
            continue;
        };
        let h = h - sea;
        *texel = if h > 0. {
            let t = (h / TALL).min(1.);
            let c = |i: usize| (SHORE[i] + (PEAK[i] - SHORE[i]) * t) as u8;
            [c(0), c(1), c(2), 255]
        } else if h > SHALLOW {
            SHALLOWS
        } else {
            continue;
        };
    }
    Some(gpui_editor_kit::texture::Texture {
        width: edge,
        height: edge,
        rgba,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use anno_formats::isd::terrain::Terrain;
    use anno_formats::vfs::FileId;

    // Both bands as the engine tests them (0x140B9DF5D-0x140B9DF7B): `4h -
    // 2703 <= 369` and `4h + 111001 <= 819`, unsigned 64-bit. Every i16
    // agrees, so the edges are the engine's: 676..=768 and
    // -27,750..=-27,546.
    #[test]
    fn the_buildable_bands_are_the_engines_tests() {
        for h in i16::MIN..=i16::MAX {
            let four = i64::from(h) * 4;
            let land = (four - 2703) as u64 <= 369;
            let under = (four + 111_001) as u64 <= 819;
            assert_eq!(buildable(h), land || under, "{h}");
            assert_eq!(BUILDABLE.contains(&h), land, "{h}");
        }
        assert!(buildable(-27_750) && buildable(-27_546));
        assert!(!buildable(-27_751) && !buildable(-27_545));
        assert_eq!(SCULPT.block_lifted_above, Some(768));
    }

    fn install() -> Option<Install> {
        let dir = anno_formats::vfs::install_dir(crate::PROFILE.env_var)?;
        Some(Install::open(&dir, crate::ARCHIVE_FAMILIES).unwrap())
    }

    // Every shipped island, as `install` serves it.
    fn islands(install: &Install) -> impl Iterator<Item = (String, Vec<u8>)> + '_ {
        (0..install.file_count()).filter_map(|i| {
            let id = FileId(i as u32);
            let path = install.file_path(id);
            path.ends_with(".isd")
                .then(|| (path.to_owned(), install.read_file(id).unwrap()))
        })
    }

    // Needs Anno 2070: a census of path blockers against gameplay heights
    // on Deep Ocean islands and surface ones, per 1 m cell (a cell is
    // blocked when any of its four 0.5 m bits is): how much ground above
    // each island's buildable band is blocked, and how much of the band
    // is open.
    #[test]
    #[ignore]
    fn path_blockers_against_the_buildable_bands() {
        use anno_formats::isd::grids::{PathBlockers, PathLayer};
        use anno_formats::isd::terrain::Terrain;
        let Some(install) = install() else { return };
        // (above blocked, above, band open, band) per kind.
        let mut census = [[0u64; 4]; 2];
        for (path, bytes) in islands(&install) {
            let doc = anno_formats::isd::Doc::parse(&bytes).unwrap();
            let under = crate::project::underwater(&install, &path).unwrap();
            let band = if under {
                UNDERWATER_BUILDABLE
            } else {
                BUILDABLE
            };
            let corners = Terrain::read(&doc, &path).unwrap().corners();
            let Ok(paths) = PathBlockers::read(&doc, &path) else {
                continue;
            };
            let (w, h) = corners.size();
            let c = &mut census[usize::from(under)];
            for z in 0..h - 1 {
                for x in 0..w - 1 {
                    let Some(v) = crate::tests::gameplay_at(&corners, x, z) else {
                        continue;
                    };
                    let blocked = (0..2).any(|dz| {
                        (0..2).any(|dx| paths.get(PathLayer::Blocked, 2 * x + dx, 2 * z + dz))
                    });
                    if v > *band.end() {
                        c[0] += u64::from(blocked);
                        c[1] += 1;
                    } else if band.contains(&v) {
                        c[2] += u64::from(!blocked);
                        c[3] += 1;
                    }
                }
            }
        }
        for (kind, c) in ["surface", "deep ocean"].iter().zip(census) {
            eprintln!(
                "{kind}: above the band {} of {} blocked ({:.1}%), band {} of {} open ({:.1}%)",
                c[0],
                c[1],
                100. * c[0] as f64 / c[1].max(1) as f64,
                c[2],
                c[3],
                100. * c[2] as f64 / c[3].max(1) as f64
            );
        }
    }

    // Needs Anno 2070: every shipped island loads into the core island and
    // saves back byte for byte untouched; its document holds a height per
    // 1 m corner and paint per 16 m chunk as the file does.
    #[test]
    fn every_shipped_island_loads_and_saves_untouched() {
        let Some(install) = install() else {
            return;
        };
        let registry = registry(&install).unwrap();
        let mut files = 0;
        for (path, bytes) in islands(&install) {
            files += 1;
            let island = from_bytes(bytes.clone().into(), &path, registry.clone(), None).unwrap();
            let doc = anno_formats::isd::Doc::parse(&bytes).unwrap();
            let (w, h) = Terrain::read(&doc, &path).unwrap().size();
            let grid = island.doc.field.grid;
            assert_eq!(
                (grid.width, grid.depth),
                (w * Terrain::CHUNK + 1, h * Terrain::CHUNK + 1),
                "{path}"
            );
            let Some(Paint::Layers(paint)) = island.doc.materials.as_ref().map(|m| &m.paint) else {
                panic!("{path}: no 2070 paint");
            };
            assert_eq!(paint.chunks(), [w, h], "{path}");
            assert!(
                write(&island, &path, None).unwrap() == bytes,
                "{path} saves changed"
            );
        }
        assert_eq!(files, 384);
    }

    /// Every id `doc`'s links name: snap partners, names' keys, object
    /// groups and walls' children.
    fn linked_ids(doc: &anno_formats::isd::Doc) -> Vec<u64> {
        use anno_formats::isd::Value;
        let mut ids = Vec::new();
        let gop = doc.root("m_GOPManager").unwrap();
        for name in ["SnapPartner", "ObjectNames"] {
            for n in doc
                .child(gop, name)
                .into_iter()
                .flat_map(|l| doc.children(l))
            {
                if let Value::Text(t) = doc.value(n)
                    && (name == "SnapPartner" || doc.name(n) == b"k")
                {
                    ids.extend(std::str::from_utf8(t).unwrap().trim().parse::<u64>());
                }
            }
        }
        for g in doc
            .child(gop, "ObjectGroups")
            .into_iter()
            .flat_map(|l| doc.children(l))
        {
            if let Value::Blob(b) = doc.value(g) {
                ids.extend(
                    b.chunks(4)
                        .map(|w| u32::from_le_bytes(w.try_into().unwrap()) as u64),
                );
            }
        }
        ids
    }

    // Needs Anno 2070: on every shipped island with two objects or more, the
    // first moves 4 m east, the second goes, and the last is copied 6 m
    // north, through the document as the editor's tools change it; a tree
    // moves, another goes, and the last tree, rock or grass is copied 4 m
    // east as Ctrl-D copies props. Saved and reopened, the moved one stands
    // where it was put (to the file's 1/4096 m), the removed one is gone
    // from the records and every link, the copies have their originals'
    // GUID, variation and turn under ids no record had, and every other
    // object and tree reads as shipped.
    #[test]
    fn object_edits_save_and_reopen_on_every_island() {
        use anno_core::island::Item;
        let Some(install) = install() else {
            return;
        };
        let registry = registry(&install).unwrap();
        let (mut files, mut edited) = (0, 0);
        for (path, bytes) in islands(&install) {
            files += 1;
            let mut island =
                from_bytes(bytes.clone().into(), &path, registry.clone(), None).unwrap();
            let loaded = island.doc.objects.clone();
            let props = island.doc.props.clone();
            if loaded.len() < 2 {
                continue;
            }
            edited += 1;
            let (first, second, last) = (&loaded[0], &loaded[1], &loaded[loaded.len() - 1]);
            let mut m = island.doc.matrix(Item::Object(first.id)).unwrap();
            m[12] += 4.;
            island.doc.set_matrix(Item::Object(first.id), m);
            let mut copy = last.copied(u64::MAX);
            copy.position[2] += 6.;
            let objects = Arc::make_mut(&mut island.doc.objects);
            objects.retain(|o| o.id != second.id);
            objects.push(copy.clone());
            if props.len() >= 2 {
                let mut copy = props[props.len() - 1].clone();
                copy.id = island.next_id;
                copy.matrix[12] += 4.;
                let trees = &mut island.doc.props;
                trees[0].matrix[14] -= 3.;
                let second = trees[1].id;
                trees.retain(|p| p.id != second);
                trees.push(copy);
            }
            let saved = write(&island, &path, None).unwrap();

            let back = from_bytes(saved.clone().into(), &path, registry.clone(), None).unwrap();
            let shipped = anno_formats::isd::Doc::parse(&bytes).unwrap();
            let largest = (anno_formats::isd::objects::read(&shipped, &path)
                .unwrap()
                .iter())
            .map(|o| o.id)
            .chain(linked_ids(&shipped))
            .max()
            .unwrap();
            let doc = anno_formats::isd::Doc::parse(&saved).unwrap();
            assert!(
                !linked_ids(&doc).contains(&second.id),
                "{path}: a link names the removed object"
            );
            let same = |a: &anno_core::island::objects::Object,
                        b: &anno_core::island::objects::Object| {
                (a.id, a.guid, a.section, a.variation, a.owner)
                    == (b.id, b.guid, b.section, b.variation, b.owner)
                    && a.position == b.position
                    && a.rotation == b.rotation
                    && a.scale == b.scale
            };
            let mut kept = back.doc.objects.iter().filter(|o| o.id <= largest);
            for o in loaded.iter().filter(|o| o.id != second.id) {
                let b = kept
                    .next()
                    .unwrap_or_else(|| panic!("{path}: {} is gone", o.id));
                if o.id == first.id {
                    let want = [o.position[0] + 4., o.position[1], o.position[2]];
                    assert!(
                        (0..3).all(|k| (b.position[k] - want[k]).abs() <= 0.5 / 4096.),
                        "{path}: {:?} for {want:?}",
                        b.position
                    );
                    assert_eq!(
                        (b.guid, b.variation, b.owner),
                        (o.guid, o.variation, o.owner),
                        "{path}"
                    );
                } else {
                    assert!(same(o, b), "{path}: {o:?} reads back as {b:?}");
                }
            }
            assert!(kept.next().is_none(), "{path}");
            let new: Vec<_> = back.doc.objects.iter().filter(|o| o.id > largest).collect();
            assert_eq!(new.len(), 1, "{path}");
            assert!(new[0].id > largest, "{path}");
            assert_eq!(
                (new[0].guid, new[0].section, new[0].variation),
                (last.guid, last.section, last.variation),
                "{path}"
            );
            assert_eq!(new[0].rotation, last.rotation, "{path}");
            let want = [last.position[0], last.position[1], last.position[2] + 6.];
            assert!(
                (0..3).all(|k| (new[0].position[k] - want[k]).abs() <= 0.5 / 4096.),
                "{path}"
            );

            if props.len() >= 2 {
                assert_eq!(back.doc.props.len(), props.len(), "{path}");
                let moved = back.doc.props[0].matrix[14];
                assert!(
                    (moved - (props[0].matrix[14] - 3.)).abs() <= 0.5 / 4096.,
                    "{path}"
                );
                // The copy of the last, the last list's, lands at its end.
                let (last, copy) = (&props[props.len() - 1], &back.doc.props[props.len() - 1]);
                let kept = back.doc.props.iter().skip(1).take(props.len() - 2);
                for (a, b) in props.iter().skip(2).zip(kept) {
                    assert_eq!(a.matrix, b.matrix, "{path}");
                }
                let kind =
                    |d: &Doc, p: &anno_core::island::Prop| d.meshes[p.source.file as usize].clone();
                assert_eq!(kind(&back.doc, copy), kind(&island.doc, last), "{path}");
                let records = anno_formats::isd::objects::read(&doc, &path).unwrap();
                let nature: Vec<_> = (records.iter())
                    .filter(|o| {
                        matches!(
                            o.section,
                            anno_formats::isd::objects::Section::Nature
                                | anno_formats::isd::objects::Section::Grass
                        )
                    })
                    .collect();
                let shipped_nature: Vec<_> = (anno_formats::isd::objects::read(&shipped, &path)
                    .unwrap()
                    .into_iter())
                .filter(|o| {
                    matches!(
                        o.section,
                        anno_formats::isd::objects::Section::Nature
                            | anno_formats::isd::objects::Section::Grass
                    )
                })
                .collect();
                let (r, was) = (nature[props.len() - 1], &shipped_nature[props.len() - 1]);
                assert!(r.id > largest, "{path}");
                assert_eq!(
                    (r.section, r.guid, r.variation),
                    (was.section, was.guid, was.variation),
                    "{path}"
                );
                // It copies its kind's first record and takes the turn and
                // scale the prop draws at, through a matrix: the same, to
                // float noise (and the record's six decimals of scale).
                let dot: f32 = (0..4).map(|k| r.rotation[k] * was.rotation[k]).sum();
                assert!(
                    dot.abs() > 1. - 1e-6,
                    "{path}: {:?} for {:?}",
                    r.rotation,
                    was.rotation
                );
                assert!((r.scale - was.scale).abs() < 1e-5, "{path}");
                let want = [was.position[0] + 4., was.position[1], was.position[2]];
                assert!(
                    (0..3).all(|k| (r.position[k] - want[k]).abs() <= 0.5 / 4096.),
                    "{path}: {:?} for {want:?}",
                    r.position
                );
            }
        }
        assert_eq!(files, 384);
        eprintln!("{edited} islands with two objects or more");
        assert!(edited > 300);
    }

    // Needs Anno 2070: n_l01, the island with the most records (11,240
    // trees and rocks, 61 buildings), loses 5,000 of its trees and rocks
    // and every other building in one save. Removals go in one pass of the
    // document, not one walk of it each, so the save takes about as long
    // as an untouched one. It reopens with exactly the records kept, as
    // shipped, and no link names a removed building. 5,000 copies of its
    // trees and rocks save as fast, and reopen 4 m east of them.
    #[test]
    fn five_thousand_removals_and_copies_save_in_linear_time_and_reopen() {
        use std::time::{Duration, Instant};
        let Some(install) = install() else {
            return;
        };
        let path = "data/levels/islands/normal/n_l01.isd";
        let bytes = install.read(path).unwrap();
        let registry = registry(&install).unwrap();
        let mut island = from_bytes(bytes.clone().into(), path, registry.clone(), None).unwrap();
        let started = Instant::now();
        assert!(write(&island, path, None).unwrap() == bytes);
        let untouched = started.elapsed();

        let (props, objects) = (island.doc.props.clone(), island.doc.objects.clone());
        assert!(props.len() > 10_000, "{}", props.len());
        let gone_prop = |k: usize| k % 2 == 0 && k < 10_000;
        let gone_object = |k: usize| k % 2 == 1;
        island.doc.props.retain(|p| !gone_prop(p.id as usize - 1));
        let mut k = 0;
        Arc::make_mut(&mut island.doc.objects).retain(|_| {
            k += 1;
            !gone_object(k - 1)
        });
        assert_eq!(island.doc.props.len(), props.len() - 5_000);
        let started = Instant::now();
        let saved = write(&island, path, None).unwrap();
        let removed = started.elapsed();
        eprintln!(
            "n_l01: untouched save {untouched:?}, with {} removals {removed:?}",
            5_000 + objects.len() / 2
        );
        // A walk of n_l01's 131,501 nodes per removal took seconds; the
        // slack covers a loaded test machine.
        assert!(
            removed < untouched * 3 + Duration::from_millis(250),
            "{removed:?} against {untouched:?} untouched"
        );

        let back = from_bytes(saved.clone().into(), path, registry.clone(), None).unwrap();
        let kept: Vec<_> = (props.iter().enumerate())
            .filter(|&(k, _)| !gone_prop(k))
            .map(|(_, p)| p.matrix)
            .collect();
        let reread: Vec<_> = back.doc.props.iter().map(|p| p.matrix).collect();
        assert!(
            kept == reread,
            "the kept trees and rocks read back as shipped"
        );
        let kept: Vec<_> = (objects.iter().enumerate())
            .filter(|&(k, _)| !gone_object(k))
            .map(|(_, o)| (o.id, o.guid, o.position, o.rotation))
            .collect();
        let reread: Vec<_> = (back.doc.objects.iter())
            .map(|o| (o.id, o.guid, o.position, o.rotation))
            .collect();
        assert_eq!(kept, reread);
        let links = linked_ids(&anno_formats::isd::Doc::parse(&saved).unwrap());
        for o in (objects.iter().enumerate()).filter(|&(k, _)| gone_object(k)) {
            assert!(!links.contains(&o.1.id), "a link names {}", o.1.id);
        }

        // As many copies, as Ctrl-D makes them: one pass too.
        let mut island = from_bytes(bytes.clone().into(), path, registry.clone(), None).unwrap();
        let trees = &mut island.doc.props;
        for k in 0..5_000 {
            let mut copy = props[k].clone();
            copy.id = (props.len() + 1 + k) as u64;
            copy.matrix[12] += 4.;
            trees.push(copy);
        }
        let started = Instant::now();
        let saved = write(&island, path, None).unwrap();
        let copied = started.elapsed();
        eprintln!("n_l01: with 5000 copies {copied:?}");
        assert!(
            copied < untouched * 3 + Duration::from_millis(250),
            "{copied:?} against {untouched:?} untouched"
        );
        let back = from_bytes(saved.into(), path, registry, None).unwrap();
        assert_eq!(back.doc.props.len(), props.len() + 5_000);
        // The copies of grass, if any, land behind the trees' copies; the
        // island's first 5,000 are all trees and rocks.
        let nature = props
            .iter()
            .take_while(|p| back.doc.props[p.id as usize - 1].matrix == p.matrix)
            .count();
        assert!(nature >= 5_000, "{nature}");
        for (k, p) in back.doc.props.iter().skip(props.len()).enumerate() {
            let want = props[k].matrix[12] + 4.;
            assert!((p.matrix[12] - want).abs() <= 0.5 / 4096., "copy {k}");
        }
    }

    // Needs Anno 2070: moving n_m02's rock 17617 changes only its
    // position, in its record and where its render parameters draw it,
    // the bytes the codec gives when set by hand.
    #[test]
    fn a_moved_rock_changes_only_its_transform_fields() {
        use anno_core::island::Item;
        use anno_formats::isd::Value;
        let Some(install) = install() else {
            return;
        };
        let path = "data/levels/islands/normal/n_m02.isd";
        let bytes = install.read(path).unwrap();
        let mut island = from_bytes(
            bytes.clone().into(),
            path,
            registry(&install).unwrap(),
            None,
        )
        .unwrap();
        let mut m = island.doc.matrix(Item::Object(17617)).unwrap();
        m[12] += 4.;
        m[14] -= 2.5;
        island.doc.set_matrix(Item::Object(17617), m);
        let saved = write(&island, path, None).unwrap();

        let mut doc = anno_formats::isd::Doc::parse(&bytes).unwrap();
        let gop = doc.root("m_GOPManager").unwrap();
        let handles = doc
            .child(
                doc.child(doc.child(gop, "Objects").unwrap(), "Handle")
                    .unwrap(),
                "Objects",
            )
            .unwrap();
        let rock = doc
            .children(handles)
            .find(|&o| doc.value(doc.child(o, "m_ID").unwrap()) == Value::Text(b"17617"))
            .unwrap();
        let p = doc.child(rock, "m_Position").unwrap();
        let Value::Blob(was) = doc.value(p) else {
            unreachable!()
        };
        let mut moved = was.to_vec();
        let x = i64::from_le_bytes(was[..8].try_into().unwrap()) + 4 * 4096;
        let z = i64::from_le_bytes(was[8..16].try_into().unwrap()) - 10_240;
        moved[..8].copy_from_slice(&x.to_le_bytes());
        moved[8..16].copy_from_slice(&z.to_le_bytes());
        doc.set(p, &moved, true).unwrap();
        let r = doc
            .child(
                doc.child(rock, "PropertyMesh").unwrap(),
                "m_RenderParameters",
            )
            .unwrap();
        let Value::Blob(was) = doc.value(r) else {
            unreachable!()
        };
        let mut drawn = was.to_vec();
        let at = drawn.len() - 48;
        drawn[at..at + 4].copy_from_slice(&(x as f32 / 4096.).to_le_bytes());
        drawn[at + 8..at + 12].copy_from_slice(&(z as f32 / 4096.).to_le_bytes());
        doc.set(r, &drawn, true).unwrap();
        assert!(
            saved == doc.encode(),
            "the save changed more than the rock's position"
        );
    }

    // Needs Anno 2070: n_m02 holds no bakery, so placing the Ecos bakery's
    // model copies the first bakery record an island ships, with that
    // record's fields and components. Put down, turned a quarter, saved
    // and reopened, it's a handle with the bakery's GUID where it was put,
    // numbered past every id the file had, and its record holds the
    // shipped one's fields in its order.
    #[test]
    fn a_building_the_island_lacks_places_from_a_shipped_record() {
        let Some(install) = install() else {
            return;
        };
        use anno_core::game::Module;
        let assets =
            anno_formats::assets::Assets::load(&install, crate::MODULE.database().unwrap())
                .unwrap();
        let cfg = "data/graphics/buildings/ecos/production/bakery_ecos/bakery_ecos.cfg";
        let path = "data/levels/islands/normal/n_m02.isd";
        let bytes = install.read(path).unwrap();
        let mut island = from_bytes(
            bytes.clone().into(),
            path,
            registry(&install).unwrap(),
            None,
        )
        .unwrap();
        let started = std::time::Instant::now();
        let bakery = crate::objects::template(&install, &assets, &island.doc.objects, cfg).unwrap();
        eprintln!("found a bakery record in {:?}", started.elapsed());
        let asset = assets.get(bakery.guid).unwrap();
        assert!(
            asset.cfgs[bakery.variation as usize]
                .replace('\\', "/")
                .eq_ignore_ascii_case(cfg)
        );
        assert!(
            island.doc.objects.iter().all(|o| o.guid != bakery.guid),
            "n_m02 has none"
        );
        assert_eq!(bakery.section, anno_core::island::objects::Section::Handle);
        let Some(anno_core::island::objects::Record::Foreign(record)) = bakery.record() else {
            panic!("{:?}", bakery.record());
        };
        let names = |doc: &anno_formats::isd::Doc, o| -> Vec<String> {
            doc.children(o)
                .map(|c| String::from_utf8_lossy(doc.name(c)).into_owned())
                .collect()
        };
        let shipped = anno_formats::isd::Doc::parse(&record).unwrap();
        let fields = names(&shipped, shipped.roots().next().unwrap());
        assert!(fields.iter().any(|f| f == "PropertyMesh"), "{fields:?}");

        let largest =
            anno_formats::isd::objects::read(&anno_formats::isd::Doc::parse(&bytes).unwrap(), path)
                .unwrap()
                .iter()
                .map(|o| o.id)
                .chain(linked_ids(&anno_formats::isd::Doc::parse(&bytes).unwrap()))
                .max()
                .unwrap();
        let mut placed = bakery.copied(1);
        placed.position = [100., 0.75, 60.];
        placed.set_direction(std::f32::consts::FRAC_PI_2);
        Arc::make_mut(&mut island.doc.objects).push(placed.clone());
        let saved = write(&island, path, None).unwrap();

        let back = from_bytes(
            saved.clone().into(),
            path,
            registry(&install).unwrap(),
            None,
        )
        .unwrap();
        let new: Vec<_> = back
            .doc
            .objects
            .iter()
            .filter(|o| o.guid == bakery.guid)
            .collect();
        assert_eq!(new.len(), 1);
        let b = new[0];
        assert_eq!(b.id, largest + 1);
        assert_eq!((b.section, b.variation), (bakery.section, bakery.variation));
        assert_eq!(b.position, [100., 0.75, 60.]);
        assert!(
            (0..4).all(|k| (b.rotation[k] - placed.rotation[k]).abs() < 1e-6),
            "{:?}",
            b.rotation
        );
        let doc = anno_formats::isd::Doc::parse(&saved).unwrap();
        let gop = doc.root("m_GOPManager").unwrap();
        let handles = doc
            .child(
                doc.child(doc.child(gop, "Objects").unwrap(), "Handle")
                    .unwrap(),
                "Objects",
            )
            .unwrap();
        let last = doc.children(handles).last().unwrap();
        assert_eq!(names(&doc, last), fields);
        assert_eq!(back.doc.objects.len(), island.doc.objects.len());
    }

    // Needs Anno 2070: the shipped-record index holds, for every asset the
    // install's islands place, its first record in file order, read back
    // the same as an independent walk of all 384 islands reads it, and
    // marks the assets at most an eighth of whose records stand off a
    // quarter turn. An
    // island open starts it on its own thread; after that, Place answers a
    // building and a tree from it, and the process reads the islands once
    // per install however often Place asks.
    #[test]
    fn the_shipped_record_index_matches_every_island_and_answers_place() {
        use crate::objects::Shipped;
        use anno_core::game::Module;
        use anno_formats::isd::objects::Object;
        let Some(install) = install() else {
            return;
        };
        // The walk: first record per GUID, its records and those off a
        // quarter turn by more than 1/100.
        let mut first: Vec<(u32, Object)> = Vec::new();
        let mut off: Vec<(u32, [usize; 2])> = Vec::new();
        let mut files = 0;
        for (path, bytes) in islands(&install) {
            files += 1;
            let doc = anno_formats::isd::Doc::parse(&bytes).unwrap();
            for o in anno_formats::isd::objects::read(&doc, &path).unwrap() {
                let d = o.direction.rem_euclid(368_640);
                let turned = d.min(368_640 - d) > 3_686;
                let turned = usize::from(turned);
                match off.iter_mut().find(|(g, _)| *g == o.guid) {
                    Some((_, [n, t])) => (*n, *t) = (*n + 1, *t + turned),
                    None => {
                        off.push((o.guid, [1, turned]));
                        first.push((o.guid, o));
                    }
                }
            }
        }
        assert_eq!(files, 384);

        let assets =
            anno_formats::assets::Assets::load(&install, crate::MODULE.database().unwrap())
                .unwrap();
        let path = "data/levels/islands/normal/n_m02.isd";
        read(&install, path, Some(&assets)).unwrap();
        let index = Shipped::of(&install).unwrap();
        assert_eq!(index.len(), first.len());
        for ((guid, want), &(_, [n, turned])) in first.iter().zip(&off) {
            let (section, bytes) = index.record(*guid).unwrap();
            assert_eq!(section, want.section, "{guid}");
            let doc = anno_formats::isd::Doc::parse(bytes).unwrap();
            let got = anno_formats::isd::objects::read_record(&doc, section).unwrap();
            assert_eq!(&got, want, "{guid}");
            assert_eq!(index.quarter_turns(*guid), turned * 8 <= n, "{guid}");
        }
        assert!(index.record(u32::MAX).is_none() && !index.quarter_turns(u32::MAX));
        let quarter = first
            .iter()
            .filter(|(g, _)| index.quarter_turns(*g))
            .count();
        eprintln!(
            "{} assets shipped, {quarter} only on quarter turns",
            index.len()
        );

        let reads = Shipped::reads();
        let island = read(&install, path, Some(&assets)).unwrap();
        let bakery = "data/graphics/buildings/ecos/production/bakery_ecos/bakery_ecos.cfg";
        let tree = (assets.iter())
            .find(|a| a.template == "Tree" && index.record(a.guid).is_some())
            .unwrap();
        let started = std::time::Instant::now();
        for cfg in [bakery, &tree.cfgs[0].replace('\\', "/")] {
            let o = crate::objects::template(&install, &assets, &island.doc.objects, cfg).unwrap();
            assert!(matches!(
                o.record(),
                Some(anno_core::island::objects::Record::Foreign(_))
            ));
        }
        eprintln!("Place found a bakery and a tree in {:?}", started.elapsed());
        assert_eq!(Shipped::reads(), reads, "answered from the index");
    }

    // Needs Anno 2070: on every shipped island without trees or rocks, a new
    // tree, rock and bush (kinds it never held, with no record of their
    // list to copy) save as copies of the shipped records Place finds for
    // their assets, which the island keeps (`Island::foreign`). Reopened,
    // each stands in its asset's list with its GUID, where it was put, and
    // the island's own records are untouched. Without those records the
    // save fails, as it did for a conversion onto such an island.
    #[test]
    fn islands_without_trees_or_rocks_save_new_ones_from_shipped_records() {
        use anno_core::game::Module;
        use anno_formats::isd::objects::Section;
        let Some(install) = install() else {
            return;
        };
        let assets =
            anno_formats::assets::Assets::load(&install, crate::MODULE.database().unwrap())
                .unwrap();
        let registry = registry(&install).unwrap();
        let cfg = |guid: u32| assets.get(guid).unwrap().cfgs[0].replace('\\', "/");
        let first = |template: &str| {
            (assets.iter())
                .find(|a| a.template == template && !a.cfgs.is_empty())
                .unwrap()
                .guid
        };
        // A tree, a rock and a bush (`Grass`, 315).
        let kinds = [
            (first("Tree"), Section::Nature),
            (first("NatureMountainObject"), Section::Nature),
            (315, Section::Grass),
        ];
        let mut bare = 0;
        for (path, bytes) in islands(&install) {
            let loaded = anno_formats::isd::Doc::parse(&bytes).unwrap();
            let records = anno_formats::isd::objects::read(&loaded, &path).unwrap();
            if records.iter().any(|o| o.section == Section::Nature) {
                continue;
            }
            bare += 1;
            let mut island =
                from_bytes(bytes.clone().into(), &path, registry.clone(), Some(&assets)).unwrap();
            let side = island.doc.field.grid.width as f32;
            let mut want = Vec::new();
            for (k, &(guid, list)) in kinds.iter().enumerate() {
                let model = cfg(guid);
                let o = crate::objects::template(&install, &assets, &island.doc.objects, &model)
                    .unwrap();
                let Some(anno_core::island::objects::Record::Foreign(record)) = o.record() else {
                    panic!("{path}: {:?}", o.record());
                };
                island.foreign.push(record);
                let at = [side / 2. + 3. * k as f32, 0.5, side / 2.];
                let file = island.doc.mesh_index(&model);
                let id = island.next_id;
                island.next_id += 1;
                let rotation = [0., 0., 0., 1.];
                island.doc.props.push(anno_core::island::Prop {
                    id,
                    matrix: anno_core::island::trs(at, rotation, [1.; 3]),
                    source: anno_formats::a7m::island::Prop {
                        file,
                        position: at,
                        rotation,
                        scale: [1.; 3],
                        color: [1.; 4],
                        adapt_terrain_height: None,
                    },
                    auto_cliff: false,
                });
                want.push((guid, list, at));
            }
            let saved = write(&island, &path, Some(&assets)).unwrap();
            let back = anno_formats::isd::objects::read(
                &anno_formats::isd::Doc::parse(&saved).unwrap(),
                &path,
            )
            .unwrap();
            assert_eq!(back.len(), records.len() + kinds.len(), "{path}");
            for (guid, list, at) in want {
                let o = (back.iter())
                    .find(|o| o.guid == guid)
                    .unwrap_or_else(|| panic!("{path}: no {guid}"));
                assert_eq!((o.section, o.position), (list, at), "{path}: {guid}");
            }
            for r in &records {
                assert!(back.contains(r), "{path}: record {} changed", r.id);
            }
        }
        eprintln!("{bare} islands without trees or rocks");
        assert!(bare > 0);
    }

    // Needs Anno 2070: on every shipped island, blocker edits made in the
    // document save the bytes the same cells set through the `.isd` codec
    // make: a strip blocked on the path layer, a disc cleared on the
    // high-ground layer (the cells whose centre lies within the brush), and where the island lists build blockers, a square added,
    // the first polygon's first point moved and its second polygon removed.
    // The save reopens with those grids, and the document as it was before
    // the edits (what undo restores) saves the shipped bytes.
    #[test]
    fn blocker_edits_save_what_the_codec_writes_and_reopen() {
        let Some(install) = install() else {
            return;
        };
        let registry = registry(&install).unwrap();
        let (mut files, mut shaped) = (0, 0);
        for (path, bytes) in islands(&install) {
            files += 1;
            let mut island =
                from_bytes(bytes.clone().into(), &path, registry.clone(), None).unwrap();
            let loaded = island.doc.clone();
            let b = island.doc.blockers().unwrap();
            let (cols, rows) = b.size();
            let mid = [cols as f32 / 4., rows as f32 / 4.];
            let square = [[2., 2.], [2., 6.], [6., 6.], [6., 2.]];
            let first = b.shapes.as_ref().filter(|s| s.len() >= 2).map(|s| {
                let p = s.points(0).next().unwrap();
                // Toward the polygon's middle keeps its winding.
                let n = s.points(0).count() as f64;
                let c = s
                    .points(0)
                    .fold([0.; 2], |a, q| [a[0] + q[0] / n, a[1] + q[1] / n]);
                [p[0] + (c[0] - p[0]) * 0.1, p[1] + (c[1] - p[1]) * 0.1]
            });
            // The 0.5 m cells whose centre lies within `r` of (x, z).
            let disc = |p: &mut PathBlockers, layer, x: f32, z: f32, r: f32, on: bool| {
                let span = |c: f32, n: usize| {
                    ((c - r) * 2.).floor().max(0.) as usize
                        ..(((c + r) * 2.).ceil() as usize + 1).min(n)
                };
                for j in span(z, rows) {
                    for i in span(x, cols) {
                        let (dx, dz) = ((i as f32 + 0.5) / 2. - x, (j as f32 + 0.5) / 2. - z);
                        if dx * dx + dz * dz <= r * r {
                            p.set(layer, i, j, on);
                        }
                    }
                }
            };
            let edit = |b: &mut PathBlockers| {
                for x in 0..8 {
                    disc(b, PathLayer::Blocked, mid[0] + x as f32, mid[1], 0.6, true);
                }
                disc(b, PathLayer::HighGround, mid[0], mid[1] + 6., 3., false);
            };

            let b = island.doc.blockers_mut().unwrap();
            for x in 0..8 {
                b.brush(PathLayer::Blocked, mid[0] + x as f32, mid[1], 0.6, true);
            }
            b.brush(PathLayer::HighGround, mid[0], mid[1] + 6., 3., false);
            if let (Some(s), Some(to)) = (b.shapes_mut(), first) {
                s.add(&square).unwrap();
                s.move_point(0, 0, to);
                s.remove(1);
                shaped += 1;
            }
            let saved = write(&island, &path, None).unwrap();

            let mut doc = anno_formats::isd::Doc::parse(&bytes).unwrap();
            let mut paths = PathBlockers::read(&doc, &path).unwrap();
            edit(&mut paths);
            paths.write(&mut doc, &path).unwrap();
            if let Some(to) = first {
                let mut s = BuildBlockers::read(&doc, &path).unwrap();
                s.add(&square).unwrap();
                s.move_point(0, 0, to);
                s.remove(1);
                s.write(&mut doc, &path).unwrap();
            }
            assert!(
                saved == doc.encode(),
                "{path}: blocker edits save otherwise"
            );

            let back = from_bytes(saved.into(), &path, registry.clone(), None).unwrap();
            let (a, b) = (back.doc.blockers().unwrap(), island.doc.blockers().unwrap());
            for layer in [PathLayer::Blocked, PathLayer::HighGround] {
                for z in 0..rows {
                    for x in 0..cols {
                        assert_eq!(a.get(layer, x, z), b.get(layer, x, z), "{path}");
                    }
                }
            }
            let shapes = |b: &Blockers| {
                b.shapes.as_ref().map(|s| {
                    (0..s.len())
                        .map(|n| s.points(n).collect::<Vec<_>>())
                        .collect::<Vec<_>>()
                })
            };
            assert_eq!(shapes(a), shapes(b), "{path}");

            island.doc = loaded;
            assert!(
                write(&island, &path, None).unwrap() == bytes,
                "{path}: undone"
            );
        }
        assert_eq!(files, 384);
        eprintln!("{shaped} islands with polygons edited");
        assert!(shaped > 0);
    }

    // Needs Anno 2070: on every shipped island, a sculpt and a dab made
    // through the core island save the same bytes as the same edits made
    // vertex by vertex: the heights through the `.isd` codec directly, the
    // paint with the brush's rule at each vertex. The sculpt lifts the
    // middle corner of the first 1 m chunk, and of the first finer chunk;
    // the dab is a 3 m brush of the first colour some chunk lacks where
    // four painted chunks meet; where the island has empty ocean, a corner
    // there is sculpted and painted too.
    #[test]
    fn island_edits_save_what_the_codec_writes() {
        let Some(install) = install() else {
            return;
        };
        let registry = registry(&install).unwrap();
        let ground = registry.textures();
        let (mut files, mut fine, mut ocean) = (0, 0, 0);
        for (path, bytes) in islands(&install) {
            files += 1;
            let loaded = anno_formats::isd::Doc::parse(&bytes).unwrap();
            let before = Terrain::read(&loaded, &path).unwrap();
            let (w, h) = before.size();
            let side = w * Terrain::CHUNK + 1;
            let middle = |k: usize| (k % w * Terrain::CHUNK + 8, k / w * Terrain::CHUNK + 8);
            // (corner, new height) edits.
            let mut sculpt = Vec::new();
            let corners = before.corners();
            let stored = crate::tests::Chunks::read(&loaded);
            let first = (0..w * h).find(|&k| stored.heights(k).is_some_and(|h| h.side == 17));
            if let Some(k) = first {
                let (x, z) = middle(k);
                sculpt.push(((x, z), corners.get(x, z).unwrap() + 1.));
            }
            if let Some(k) = (0..w * h).find(|&k| stored.heights(k).is_some_and(|h| h.side > 17)) {
                let (x, z) = middle(k);
                sculpt.push(((x + 1, z), corners.get(x + 1, z).unwrap() - 0.5));
                fine += 1;
            }
            let empty = (0..w * h).find(|&k| stored.heights(k).is_none());
            if let Some(k) = empty {
                sculpt.push((middle(k), -35.));
                ocean += 1;
            }
            let painted = |k: usize| !before.ids(k).is_empty();
            let vertex = (0..w * h)
                .find(|&k| {
                    k % w + 1 < w
                        && k / w + 1 < h
                        && [k, k + 1, k + w, k + w + 1].into_iter().all(painted)
                })
                .map(|k| ((k % w + 1) * Terrain::CHUNK, (k / w + 1) * Terrain::CHUNK))
                .unwrap_or_else(|| middle((0..w * h).find(|&k| painted(k)).unwrap()));
            let colour = (ground.textures().iter())
                .filter(|t| t.id != 0 && !ground.scatter(t.id))
                .map(|t| t.id)
                .find(|&id| {
                    anno_formats::isd::terrain::chunks_at((w, h), vertex.0, vertex.1)
                        .any(|k| painted(k) && !before.ids(k).contains(&id))
                })
                .unwrap_or(ground.textures()[1].id);
            // A cone 3 m wide; the same weights on both sides.
            let dabs: Vec<([usize; 2], u32)> = [vertex]
                .into_iter()
                .chain(empty.map(middle))
                .map(|v| ([v.0, v.1], colour))
                .collect();
            let weight = |c: [usize; 2], p: [f32; 2]| {
                (1. - (p[0] - c[0] as f32).hypot(p[1] - c[1] as f32) / 3.).max(0.)
            };

            // Through the island.
            let mut island =
                from_bytes(bytes.clone().into(), &path, registry.clone(), None).unwrap();
            let field = Arc::make_mut(&mut island.doc.field);
            for &((x, z), v) in &sculpt {
                field.heights[z * side + x] = v;
            }
            let m = island.doc.materials.as_mut().unwrap();
            for &(c, id) in &dabs {
                let (lo, hi) = (c.map(|v| v as f32 - 3.), c.map(|v| v as f32 + 3.));
                m.paint_with(lo, hi, id as u8, |p| weight(c, p));
            }
            let saved = write(&island, &path, None).unwrap();

            // Vertex by vertex.
            let mut doc = anno_formats::isd::Doc::parse(&bytes).unwrap();
            let mut corners = before.corners();
            let mut layers = Layers::read(&before, registry.clone()).unwrap();
            for &((x, z), v) in &sculpt {
                corners.set(x, z, v);
            }
            for &(c, id) in &dabs {
                for z in c[1].saturating_sub(3)..(c[1] + 4).min(side) {
                    for x in c[0].saturating_sub(3)..(c[0] + 4).min(side) {
                        let a = weight(c, [x as f32, z as f32]);
                        if a > 0. {
                            let _ = layers.paint(x, z, id as u8, a);
                        }
                    }
                }
            }
            // The island's path blockers are the document's: blocking what
            // a sculpt lifts is the editor's tools' (src/island/grids.rs),
            // and this edit went past them.
            let sculpt = Sculpt {
                block_lifted_above: None,
                ..SCULPT
            };
            corners.write(&mut doc, &path, sculpt).unwrap();
            write_paint(&mut doc, &path, &layers).unwrap();
            assert!(
                saved == doc.encode(),
                "{path}: the island's edit saves otherwise"
            );
            assert!(saved != bytes, "{path}: the edit changed nothing");
        }
        assert_eq!(files, 384);
        eprintln!("{fine} islands with finer chunks, {ocean} with empty ocean");
        assert!(fine > 0 && ocean > 0);
    }
}
