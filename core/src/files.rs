// The kinds of file a game's install holds, as data: how each is recognised,
// grouped and viewed. Games list their own kinds (`Module::files`), core the
// generic ones (`GENERIC`) and the Anno formats every game here shares. A
// file no kind claims is still shown, as text or hex, by sniffing its bytes.

use crate::game::Module;
use gpui_editor_kit::texture::Texture;

/// The Assets filter chip a kind lists under, which also picks its icon and
/// what the footer calls it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Group {
    Textures,
    Models,
    Islands,
    Ui,
    Text,
}

/// How a file of the kind previews, and the workspace that opens it when the
/// game has that capability.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum View {
    /// A DDS texture: read lazily, one mip at a time.
    Texture,
    /// PNG or JPEG, decoded by the UI's own image loader.
    Image,
    /// A small picture in some other format: the kind's `decode`, or the
    /// shell's Windows cursor reader when it has none.
    Picture,
    /// Drawn in 3D.
    Model,
    /// Opens in the island editor where the game has `islands()`.
    Island,
    /// Opens in the map editor where the game has `islands().maps()`.
    Map,
    /// Opens in the UI editor where the game has `ui()`.
    Screen,
}

/// A picture format's reader: the image and its facts line for the preview
/// ("64×64 · 1-bit mask" is the image's size, then this).
pub type Decode = fn(&[u8]) -> anyhow::Result<(Texture, String)>;

/// The files a file names besides itself, as the game reads them (shipped
/// meshes and textures, with the levels and mips it finds by name), from
/// its bytes. Not checked against any install: callers keep the ones that
/// exist where they look. A tag scan, not a parse: it also names animation
/// meshes, and skips a name with a space in it (one shipped 117 mesh).
pub type Deps = fn(&[u8]) -> Vec<String>;

/// One kind of file.
#[derive(Clone, Copy)]
pub struct Kind {
    pub id: &'static str,
    pub label: &'static str,
    pub group: Group,
    /// Extensions without the dot, lower case.
    pub exts: &'static [&'static str],
    /// A folder whose files are this kind whatever their extension (UI
    /// screens are extensionless, named by GUID): `"/ui/studio/generated/"`.
    pub folder: Option<&'static str>,
    /// `None` previews as text or hex, by sniffing.
    pub view: Option<View>,
    pub decode: Option<Decode>,
    pub deps: Option<Deps>,
}

impl Kind {
    /// A kind matched by extension alone, with no picture decoder.
    pub const fn ext(
        id: &'static str,
        label: &'static str,
        group: Group,
        exts: &'static [&'static str],
        view: Option<View>,
    ) -> Kind {
        Kind {
            id,
            label,
            group,
            exts,
            folder: None,
            view,
            decode: None,
            deps: None,
        }
    }

    pub fn matches(&self, path: &str) -> bool {
        let ext = extension(path);
        self.exts.iter().any(|e| e.eq_ignore_ascii_case(ext))
            || self.folder.is_some_and(|f| path.contains(f))
    }
}

/// What any game may hold, whoever made it. Tried after the game's own.
pub const GENERIC: &[Kind] = &[
    Kind::ext(
        "dds",
        "DDS texture",
        Group::Textures,
        &["dds"],
        Some(View::Texture),
    ),
    Kind::ext(
        "png",
        "PNG image",
        Group::Textures,
        &["png"],
        Some(View::Image),
    ),
    Kind::ext(
        "jpeg",
        "JPEG image",
        Group::Textures,
        &["jpg", "jpeg"],
        Some(View::Image),
    ),
    Kind::ext(
        "cur",
        "Windows cursor",
        Group::Textures,
        &["cur"],
        Some(View::Picture),
    ),
    Kind::ext(
        "ani",
        "Animated cursor",
        Group::Textures,
        &["ani"],
        Some(View::Picture),
    ),
    // Listed under Text; still sniffed, so a binary `.txt` shows as hex.
    Kind::ext(
        "text",
        "Text",
        Group::Text,
        &["xml", "json", "txt", "ini", "lua", "fx", "fxh", "hlsl"],
        None,
    ),
];

// The Anno formats all three games share. Core keeps them so a game's list
// names them instead of spelling them out again; a game without them simply
// leaves them out.

/// A model: its meshes, materials and props (`formats::model`).
pub const CFG: Kind = Kind {
    deps: Some(model_deps),
    ..Kind::ext("cfg", "Model", Group::Models, &["cfg"], Some(View::Model))
};
/// A mesh, drawn with the materials of the cfg that owns it.
pub const RDM: Kind = Kind::ext("rdm", "Mesh", Group::Models, &["rdm"], Some(View::Model));
/// A model's animation and state rules, beside its cfg.
pub const FC: Kind = Kind::ext("fc", "Feedback", Group::Models, &["fc"], None);
/// A model's bounds and blockers, beside its cfg.
pub const IFO: Kind = Kind::ext("ifo", "Model info", Group::Text, &["ifo"], None);

/// How the shell's model viewer and the library's thumbnails draw this
/// game's models outside an island (by day, in one player's colour), and
/// which props it brings in from files or other games. Module data, not
/// `scene()`, because a game previews models before it has a scene (2070).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ModelLook {
    /// What dye masks blend toward, linear (`PlayerColor::dye`).
    pub dye: [f32; 3],
    /// Scales models' own light (`GlowBrightness`).
    pub glow: f32,
    /// Models glow from a G-buffer (117, 2070): the glow takes the albedo
    /// as stored, without the cfg's emissive colour, whose grey scales the
    /// night mask instead, kept in 31 steps. 1800 shades forward: albedo
    /// texture times emissive colour.
    pub deferred_glow: bool,
    /// The wall the engine shows behind every `WINDOW_INTERIOR_ENABLED`
    /// window (`cWindowInteriorTex`, a global it binds itself).
    pub window_interior: Option<&'static str>,
    /// The game's own surface families for its models; `None` draws them
    /// with the kit's.
    pub surfaces: Option<SurfacesOf>,
    /// The colours players own buildings in, the first the one an island
    /// without an owner shows (View settings) and `dye` is.
    pub player_colors: &'static [crate::game::PlayerColor],
    /// How the game draws decals (`gpu::Decal`).
    pub decals: DecalLook,
}

/// How a game draws decals: how far toward the camera, and whether a
/// model's box decals lie on the ground and draw with it, before every
/// model.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DecalLook {
    /// Metres a box decal (a cfg's `Decals`) moves toward the camera.
    pub box_bias: f32,
    /// The move grows at grazing angles (117's camera-angle bias) rather
    /// than staying the bias along the line of sight (1800).
    pub grazing: bool,
    /// Box decals draw right after the ground and before the models, as
    /// the game's frame orders them; otherwise over everything opaque.
    pub box_on_ground: bool,
    /// Mesh decals light as the ground under them: 117 shades its G-buffer
    /// after they blend in, and PackGBufferAlphaBlend leaves the material
    /// tag below (terrain, so matte specular fades). 1800's model_decal.fx
    /// lights them itself, specular whole.
    pub mesh_as_ground: bool,
}

/// Which models a game brings in as its own props, beside its
/// `read_model` and `write_model`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Ports {
    /// The games whose props this game can bring in as its own, through
    /// the neutral model (1800 prps as 117 cfgs, 117 cfgs as 1800 prps,
    /// 2070 cfgs as either). Only props: 1800 cfgs (decoration objects)
    /// may use effects and shaders 117 lacks, and a prp holds one static
    /// mesh.
    pub props_from: &'static [crate::game::Game],
    /// A prop model's extension: what the Models page lists from
    /// dependency mods.
    pub prop_ext: &'static str,
    /// Add model takes a bare `.rdm` and dresses it in a shipped template
    /// cfg; 1800 props need their `.prp`.
    pub mesh_import: bool,
}

impl Ports {
    /// Bringing in no props.
    pub const NONE: Ports = Ports {
        props_from: &[],
        prop_ext: "",
        mesh_import: false,
    };
}

/// A game's own surface families for its models, which its model look
/// names: the registry its renderers draw with, and how a material's mesh
/// joins one of them.
pub trait Surfaces: Sync {
    /// The families models draw in outside an island: previews and
    /// thumbnails, under the kit's lighting.
    fn families(&self) -> std::sync::Arc<gpui_editor_kit::gpu::Families>;
    /// The same families under the game's own scene lighting (its `shade`
    /// and fog), for islands lit by one of its presets; `None` draws them
    /// with `families`.
    fn lit(&self) -> Option<std::sync::Arc<gpui_editor_kit::gpu::Families>> {
        None
    }
    /// Puts `data`, a lit mesh of `material` drawn with `look`, in one of
    /// the families with its payload and slots, or leaves it the kit's.
    /// `texture` reads a texture by its path, sized for the mesh.
    fn dress(
        &self,
        look: &ModelLook,
        material: &anno_formats::model::Material,
        data: &mut gpui_editor_kit::gpu::MeshData,
        texture: &mut dyn FnMut(&str) -> Option<gpui_editor_kit::texture::Image>,
    );
    /// `glow` as renderers of these families take it: its strength the
    /// kit's, the rest in the host words the families read, in their own
    /// layout. By default none: the kit's families read no host words.
    fn glow(&self, glow: &crate::ambient::ModelGlow) -> gpui_editor_kit::gpu::Glow {
        gpui_editor_kit::gpu::Glow {
            strength: glow.strength,
            host: [0.; gpui_editor_kit::gpu::HOST_WORDS],
        }
    }
}

/// A game's [`Surfaces`], compared by address.
#[derive(Clone, Copy)]
pub struct SurfacesOf(pub &'static dyn Surfaces);

impl std::fmt::Debug for SurfacesOf {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SurfacesOf(..)")
    }
}

impl PartialEq for SurfacesOf {
    fn eq(&self, other: &Self) -> bool {
        std::ptr::addr_eq(self.0, other.0)
    }
}

impl ModelLook {
    /// Undyed, the kit's glow strength, forward glow and no rooms: what a
    /// game that says nothing about its models gets.
    pub const PLAIN: ModelLook = ModelLook {
        dye: [1.; 3],
        glow: 1.,
        deferred_glow: false,
        window_interior: None,
        surfaces: None,
        player_colors: &[],
        decals: DecalLook {
            box_bias: 0.2,
            grazing: true,
            box_on_ground: false,
            mesh_as_ground: false,
        },
    };

    /// The families the look's meshes draw in (`Surfaces::families`), the
    /// kit's alone without surfaces of its own: what a renderer of its
    /// models takes (`Renderer::with_families`).
    pub fn families(&self) -> std::sync::Arc<gpui_editor_kit::gpu::Families> {
        self.surfaces
            .map_or_else(gpui_editor_kit::gpu::Families::kit, |s| s.0.families())
    }

    /// `glow` in the host words of the look's families
    /// (`Surfaces::glow`), the kit's layout without surfaces of its own.
    pub fn glow(&self, glow: &crate::ambient::ModelGlow) -> gpui_editor_kit::gpu::Glow {
        match self.surfaces {
            Some(s) => s.0.glow(glow),
            None => gpui_editor_kit::gpu::Glow {
                strength: glow.strength,
                host: [0.; gpui_editor_kit::gpu::HOST_WORDS],
            },
        }
    }

    /// The scene-wide half of the look. Night masks stay dark: previews
    /// show the day.
    pub fn scene_glow(&self) -> crate::ambient::ModelGlow {
        crate::ambient::ModelGlow {
            tint: self.dye,
            strength: self.glow,
            ..Default::default()
        }
    }
}

/// What a model text (a cfg, an 1800 prp) reads: every file it names, as
/// shipped, each with its siblings. In reference order, without repeats.
pub fn model_deps(bytes: &[u8]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for r in references(&String::from_utf8_lossy(bytes)) {
        for f in siblings(&shipped_path(&r)) {
            if !out.contains(&f) {
                out.push(f);
            }
        }
    }
    out
}

/// Extensions a model names files with: meshes as exported (fbx, max) or
/// shipped, textures as authored or shipped, and sub-models and effects.
const REFERENCED: [&str; 14] = [
    "rdm", "fbx", "max", "dds", "psd", "png", "tga", "cfg", "prp", "rdp", "anim", "cf7", "ifo",
    "fc",
];

/// File references in a model's XML: every tag body that is a path with
/// one of the `REFERENCED` extensions, as written, without repeats.
pub fn references(xml: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for body in xml.split('>').filter_map(|s| s.split('<').next()) {
        let body = body.trim();
        let Some((_, ext)) = body.rsplit_once('.') else {
            continue;
        };
        let ok = REFERENCED.contains(&ext.to_ascii_lowercase().as_str())
            && !body.contains(char::is_whitespace)
            && body.len() > ext.len() + 1;
        if ok && !out.iter().any(|o| o == body) {
            out.push(body.to_owned());
        }
    }
    out
}

/// The file the game reads for reference `r`: the shipped mesh for an
/// exporter path, the top mip for an authored texture, else `r` itself,
/// normalised.
pub fn shipped_path(r: &str) -> String {
    use anno_formats::model::{mesh_path, texture_path};
    match extension(r).to_ascii_lowercase().as_str() {
        "rdm" | "fbx" | "max" => mesh_path(r),
        "dds" | "psd" | "png" | "tga" => texture_path(r),
        _ => anno_formats::rda::normalize_path(r).into_owned(),
    }
}

/// `file` and the siblings the game finds by name, in level order: a
/// mesh's distance levels `_lod1` to `_lod4` (the games' writers take the
/// index as the level), a top mip's lower mips `_1` to `_4`. Only `_lod0`
/// is taken off a mesh name; a deeper level gets no siblings that exist.
pub fn siblings(file: &str) -> Vec<String> {
    let mut out = vec![file.to_owned()];
    if let Some(stem) = file.strip_suffix(".rdm") {
        let stem = stem.strip_suffix("_lod0").unwrap_or(stem);
        out.extend((1..=4).map(|k| format!("{stem}_lod{k}.rdm")));
    } else if let Some(stem) = file.strip_suffix("_0.dds") {
        out.extend((1..=4).map(|k| format!("{stem}_{k}.dds")));
    }
    out
}

/// The mesh family any level of a mesh belongs to: `…/olive_01_lod2.rdm`
/// and `…/olive_01.rdm` are both `…/olive_01`.
pub fn mesh_stem(rdm: &str) -> &str {
    let stem = rdm.strip_suffix(".rdm").unwrap_or(rdm);
    match stem.rsplit_once("_lod") {
        Some((base, n)) if n.len() == 1 && n.as_bytes()[0].is_ascii_digit() => base,
        _ => stem,
    }
}

/// The kind `module` reads `path` as: the game's own kinds first, then
/// `GENERIC`. The first match wins, so a game can claim a generic extension.
pub fn kind_of(module: &dyn Module, path: &str) -> Option<&'static Kind> {
    module
        .files()
        .iter()
        .chain(GENERIC)
        .find(|k| k.matches(path))
}

/// The text after the file name's last dot; empty for none.
pub fn extension(path: &str) -> &str {
    let name = path.rsplit('/').next().unwrap_or(path);
    name.rsplit_once('.').map_or("", |(_, ext)| ext)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn siblings_list_levels_and_mips_by_name() {
        assert_eq!(
            siblings("a/rock_lod0.rdm"),
            [
                "a/rock_lod0.rdm",
                "a/rock_lod1.rdm",
                "a/rock_lod2.rdm",
                "a/rock_lod3.rdm",
                "a/rock_lod4.rdm"
            ]
        );
        assert_eq!(siblings("a/rock.rdm")[1], "a/rock_lod1.rdm");
        assert_eq!(siblings("a/bark_0.dds")[4], "a/bark_4.dds");
        assert_eq!(siblings("a/bark.dds"), ["a/bark.dds"]);
        assert_eq!(
            mesh_stem("data/trees/olive/rdm/olive_01_lod2.rdm"),
            "data/trees/olive/rdm/olive_01"
        );
        assert_eq!(
            mesh_stem("data/rocks/rock_06_a.rdm"),
            "data/rocks/rock_06_a"
        );
    }

    #[test]
    fn a_model_reads_its_meshes_and_textures_as_shipped() {
        let cfg = br"<Config><FileName>Data\Graphics\X\rdm\Tree_lod0.FBX</FileName>
            <cModelDiffTex>data/graphics/x/maps/bark_diff.psd</cModelDiffTex>
            <Name>not a.path here</Name><Again>data/graphics/x/maps/bark_diff.psd</Again>
            <FileName>data/graphics/x/fx/smoke.rdp</FileName></Config>";
        let deps = (CFG.deps.unwrap())(cfg);
        assert_eq!(
            deps,
            [
                "data/graphics/x/rdm/tree_lod0.rdm",
                "data/graphics/x/rdm/tree_lod1.rdm",
                "data/graphics/x/rdm/tree_lod2.rdm",
                "data/graphics/x/rdm/tree_lod3.rdm",
                "data/graphics/x/rdm/tree_lod4.rdm",
                "data/graphics/x/maps/bark_diff_0.dds",
                "data/graphics/x/maps/bark_diff_1.dds",
                "data/graphics/x/maps/bark_diff_2.dds",
                "data/graphics/x/maps/bark_diff_3.dds",
                "data/graphics/x/maps/bark_diff_4.dds",
                "data/graphics/x/fx/smoke.rdp",
            ]
        );
    }

    // The three rules `deps` folded, as they were, to check the fold
    // against shipped models.
    mod before {
        pub fn neutral_mesh_family(mesh: &str) -> Vec<String> {
            let stem = mesh.strip_suffix(".rdm").unwrap_or(mesh);
            let stem = stem.strip_suffix("_lod0").unwrap_or(stem);
            let mut out = vec![mesh.to_owned()];
            out.extend((1..=4).map(|level| format!("{stem}_lod{level}.rdm")));
            out
        }

        pub fn browser_mesh_family(rdm: &str) -> &str {
            let stem = rdm.strip_suffix(".rdm").unwrap_or(rdm);
            match stem.rsplit_once("_lod") {
                Some((base, n)) if n.len() == 1 && n.as_bytes()[0].is_ascii_digit() => base,
                _ => stem,
            }
        }

        const REFERENCED: [&str; 14] = [
            "rdm", "fbx", "max", "dds", "psd", "png", "tga", "cfg", "prp", "rdp", "anim", "cf7",
            "ifo", "fc",
        ];

        /// port.rs's `project_models` list, before its sort.
        pub fn port_files(xml: &str) -> Vec<String> {
            use anno_formats::model::{mesh_path, texture_path};
            let mut refs: Vec<String> = Vec::new();
            for body in xml.split('>').filter_map(|s| s.split('<').next()) {
                let body = body.trim();
                let Some((_, ext)) = body.rsplit_once('.') else {
                    continue;
                };
                let ok = REFERENCED.contains(&ext.to_ascii_lowercase().as_str())
                    && !body.contains(char::is_whitespace)
                    && body.len() > ext.len() + 1;
                if ok && !refs.iter().any(|o| o == body) {
                    refs.push(body.to_owned());
                }
            }
            let mut out = Vec::new();
            for r in refs {
                let ext = r
                    .rsplit_once('.')
                    .map_or("", |(_, e)| e)
                    .to_ascii_lowercase();
                let wanted = match ext.as_str() {
                    "rdm" | "fbx" | "max" => mesh_path(&r),
                    "dds" | "psd" | "png" | "tga" => texture_path(&r),
                    _ => anno_formats::rda::normalize_path(&r).into_owned(),
                };
                out.push(wanted.clone());
                if let Some(stem) = wanted.strip_suffix(".rdm") {
                    let stem = stem.strip_suffix("_lod0").unwrap_or(stem);
                    out.extend((1..=4).map(|k| format!("{stem}_lod{k}.rdm")));
                } else if let Some(stem) = wanted.strip_suffix("_0.dds") {
                    out.extend((1..=4).map(|k| format!("{stem}_{k}.dds")));
                }
            }
            out
        }
    }

    // Needs the games: over every shipped 117 and 1800 model, the game
    // writers' mesh levels, the browser's owner test and a project model's
    // files are what the three old rules gave.
    #[test]
    fn deps_give_the_old_lists_on_shipped_models() {
        use anno_formats::model::Model;
        use anno_formats::vfs::{FileId, Install};
        for var in ["ANNO117_DIR", "ANNO1800_DIR"] {
            let Some(dir) = anno_formats::vfs::install_dir(var) else {
                eprintln!("skip: {var} unset");
                continue;
            };
            // 117's and 1800's mount order, as their game crates give it:
            // core's tests can't reach those.
            let install = Install::open(dir, &["data"]).unwrap();
            let (mut models, mut meshes) = (0, 0);
            for id in (0..install.file_count() as u32).map(FileId) {
                let path = install.file_path(id);
                if !(path.ends_with(".cfg") || path.ends_with(".prp")) {
                    continue;
                }
                let Ok(bytes) = install.read(&path) else {
                    continue;
                };
                // A project model's files, as port.rs listed them.
                let mut old = before::port_files(&String::from_utf8_lossy(&bytes));
                let mut new = model_deps(&bytes);
                old.sort();
                old.dedup();
                new.sort();
                assert_eq!(new, old, "{path}");
                models += 1;
                let Ok(model) = Model::parse(&bytes) else {
                    continue;
                };
                for part in model.parts.iter().filter(|p| p.rdm.ends_with(".rdm")) {
                    // The writers' levels, and the browser's owner test.
                    assert_eq!(siblings(&part.rdm), before::neutral_mesh_family(&part.rdm));
                    assert_eq!(mesh_stem(&part.rdm), before::browser_mesh_family(&part.rdm));
                    meshes += 1;
                }
            }
            eprintln!("{var}: {models} models, {meshes} meshes");
            assert!(
                models > 1000 && meshes > 1000,
                "{models} models, {meshes} meshes"
            );
        }
    }
}
