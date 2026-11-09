// Model configs (`.cfg`): which rdm meshes make up a graphics object, their
// materials and textures, and where sub-models, nested prop cfgs, decals and
// particles sit.
//
// Cfg is Anno's XML dialect: no attributes or declaration, tag names with
// spaces and brackets (`<Common [ModelCommon]>`), `</>` closers in some
// tools' output, and the odd garbage byte in unused strings. Parsed into a
// small tree; files are at most a few hundred KB.
use anyhow::{Context, Result};

use crate::rda::normalize_path;
use crate::rdm::Rdm;
use crate::vfs::Install;

/// Local placement of a part relative to the cfg's origin. Y up, metres.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Transform {
    pub position: [f32; 3],
    /// Unit quaternion `[x, y, z, w]`.
    pub rotation: [f32; 4],
    /// `Scale` times `NonUniformScale`.
    pub scale: [f32; 3],
}

impl Default for Transform {
    fn default() -> Self {
        Transform::IDENTITY
    }
}

impl Transform {
    pub const IDENTITY: Transform = Transform {
        position: [0.0; 3],
        rotation: [0.0, 0.0, 0.0, 1.0],
        scale: [1.0; 3],
    };

    /// Scale, then rotate, then translate.
    pub fn apply(&self, p: [f32; 3]) -> [f32; 3] {
        let s = [
            p[0] * self.scale[0],
            p[1] * self.scale[1],
            p[2] * self.scale[2],
        ];
        let r = rotate(self.rotation, s);
        [
            r[0] + self.position[0],
            r[1] + self.position[1],
            r[2] + self.position[2],
        ]
    }

    /// Column-major 4x4 matrix of [`Transform::apply`].
    pub fn matrix(&self) -> [f32; 16] {
        let x = rotate(self.rotation, [self.scale[0], 0.0, 0.0]);
        let y = rotate(self.rotation, [0.0, self.scale[1], 0.0]);
        let z = rotate(self.rotation, [0.0, 0.0, self.scale[2]]);
        let t = self.position;
        [
            x[0], x[1], x[2], 0.0, y[0], y[1], y[2], 0.0, z[0], z[1], z[2], 0.0, t[0], t[1], t[2],
            1.0,
        ]
    }

    /// `self` applied after `inner`. Exact for uniform scale; non-uniform
    /// scale under rotation is not representable and is approximated.
    pub fn then(&self, inner: &Transform) -> Transform {
        Transform {
            position: self.apply(inner.position),
            rotation: quat_mul(self.rotation, inner.rotation),
            scale: [0, 1, 2].map(|k| self.scale[k] * inner.scale[k]),
        }
    }
}

fn rotate(q: [f32; 4], v: [f32; 3]) -> [f32; 3] {
    // v + 2w(u x v) + 2(u x (u x v)), u = q.xyz
    let u = [q[0], q[1], q[2]];
    let cross = |a: [f32; 3], b: [f32; 3]| {
        [
            a[1] * b[2] - a[2] * b[1],
            a[2] * b[0] - a[0] * b[2],
            a[0] * b[1] - a[1] * b[0],
        ]
    };
    let t = cross(u, v).map(|c| 2.0 * c);
    let c = cross(u, t);
    [0, 1, 2].map(|k| v[k] + q[3] * t[k] + c[k])
}

fn quat_mul(a: [f32; 4], b: [f32; 4]) -> [f32; 4] {
    [
        a[3] * b[0] + a[0] * b[3] + a[1] * b[2] - a[2] * b[1],
        a[3] * b[1] - a[0] * b[2] + a[1] * b[3] + a[2] * b[0],
        a[3] * b[2] + a[0] * b[1] - a[1] * b[0] + a[2] * b[3],
        a[3] * b[3] - a[0] * b[0] - a[1] * b[1] - a[2] * b[2],
    ]
}

/// How a material composites, decided by its shader (see the doc for the
/// ShaderID table).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Alpha {
    Opaque,
    /// Alpha test against the diffuse alpha at this reference
    /// (`cShadowAlphaRef`). Plant shaders 21, 22, 23.
    Cutout(f32),
    /// Alpha blended at `cOpacity` times texture alpha: hair (12) and the
    /// opacity shaders (19).
    Blend(f32),
    /// Projected onto what is below (mesh decals 1, 6, 26; terrain decal 2).
    Decal,
    /// Not drawn: terrain and water cutout volumes (4, 5).
    Hidden,
}

/// One `<Materials>` entry. Index `i` matches [`crate::rdm::MaterialGroup::material`].
#[derive(Debug, Clone, PartialEq)]
pub struct Material {
    pub name: String,
    pub shader: u32,
    /// The format the material expects, e.g. `P4h_N4b_G4b_B4b_T2h_C4b`.
    pub vertex_format: String,
    /// Shipped `.dds` path of `cModelDiffTex`, `None` when `DIFFUSE_ENABLED`
    /// is 0 or the cfg has none.
    pub diffuse: Option<String>,
    /// `cModelNormalTex`, `None` when `NORMAL_ENABLED` is 0.
    pub normal: Option<String>,
    /// `cModelMetallicTex`, `None` when `METALLIC_TEX_ENABLED` is 0.
    pub metallic: Option<String>,
    /// `cDiffuseColor`; multiplies the diffuse texture, values above 1 occur.
    pub diffuse_color: [f32; 3],
    pub double_sided: bool,
    /// `cTerrainTintIntensity` when `cUseTerrainTinting` is 1, else 0: how
    /// much the ground's tint map multiplies the albedo in game.
    pub terrain_tint: f32,
    pub alpha: Alpha,
}

/// One `<Models>` entry: an rdm placed in the object.
#[derive(Debug, Clone)]
pub struct Part {
    pub name: String,
    /// Shipped `.rdm` path (the cfg names the `.fbx` source).
    pub rdm: String,
    /// Loaded by [`Model::load`]; `None` if missing or unreadable, see
    /// [`Model::problems`].
    pub mesh: Option<Rdm>,
    pub transform: Transform,
    pub materials: Vec<Material>,
    /// `AABBMin`/`AABBMax`, mesh-local. Equals the rdm's stored bounds.
    pub aabb: Option<([f32; 3], [f32; 3])>,
    /// Has `<Animations>` or an `ANIMATION_TRANSFORM` (spinning, swaying).
    /// The static pose is still a valid preview.
    pub animated: bool,
    /// Attached to a bone of another part (`BONELINK_TRANSFORM`); the local
    /// transform is relative to that bone, so the static placement is off.
    pub bone_linked: bool,
}

/// One `<Files>` entry: another cfg placed in this one.
#[derive(Debug, Clone, PartialEq)]
pub struct Prop {
    pub name: String,
    /// Normalized cfg path, load it with [`Model::load`].
    pub cfg: String,
    pub transform: Transform,
}

/// One `<Decals>` entry: a box projector. The box spans `±extents` around
/// the transform's origin; textures project down the Y axis.
#[derive(Debug, Clone, PartialEq)]
pub struct Decal {
    pub transform: Transform,
    pub extents: [f32; 3],
    /// `TexCoords` as `[u0, v0, u1, v1]`.
    pub uv_rect: [f32; 4],
    /// `Terrain`, `Mesh`, ... as written.
    pub kind: String,
    pub material: Option<Material>,
}

/// One `<Particles>` entry.
#[derive(Debug, Clone, PartialEq)]
pub struct Particle {
    /// `.rdp` path.
    pub file: String,
    pub transform: Transform,
}

/// A parsed `.cfg`.
#[derive(Debug, Clone, Default)]
pub struct Model {
    /// Normalized cfg path; empty for [`Model::parse`].
    pub path: String,
    /// Object bounds, `MeshCenter ± MeshExtent`, covering all parts.
    pub mesh_center: [f32; 3],
    pub mesh_extent: [f32; 3],
    pub parts: Vec<Part>,
    pub props: Vec<Prop>,
    pub decals: Vec<Decal>,
    pub particles: Vec<Particle>,
    /// Missing or broken referenced files. A model with problems is still
    /// usable; the editor draws what loaded.
    pub problems: Vec<String>,
}

impl Model {
    /// Parses a cfg and maps source paths to shipped names (`.fbx` -> `.rdm`,
    /// textures -> `<stem>_0.dds`) without checking they exist.
    pub fn parse(cfg: &[u8]) -> Result<Model> {
        let root = Node::parse(cfg);
        if let Some(prop) = root.child("Prop") {
            let materials = prop
                .child("Materials")
                .into_iter()
                .flat_map(|n| &n.children)
                .map(|c| {
                    let mut m = material(c);
                    m.vertex_format = prop.text("VertexFormat").into();
                    if prop.text("Type") == "Plant" {
                        m.alpha = Alpha::Cutout(c.f32("cAlphaRef").unwrap_or(0.5));
                    }
                    m
                })
                .collect();
            return Ok(Model {
                parts: vec![Part {
                    name: prop.text("Type").into(),
                    rdm: mesh_path(prop.text("MeshFileName")),
                    mesh: None,
                    transform: Transform::IDENTITY,
                    materials,
                    aabb: None,
                    animated: false,
                    bone_linked: false,
                }],
                ..Model::default()
            });
        }
        let main = root
            .configs()
            .find(|c| c.config_type() == Some("MAIN"))
            .or_else(|| root.child("Config"))
            .context("cfg has no MAIN Config")?;
        let mut model = Model {
            mesh_center: main.vec3("MeshCenter").unwrap_or_default(),
            mesh_extent: main.vec3("MeshExtent").unwrap_or_default(),
            ..Model::default()
        };
        for c in main.list("Models") {
            let (transform, animated, bone_linked) = transformer(c);
            let aabb = match (c.child("AABBMin"), c.child("AABBMax")) {
                (Some(lo), Some(hi)) => Some((lo.xyz(), hi.xyz())),
                _ => None,
            };
            model.parts.push(Part {
                name: c.text("Name").to_string(),
                rdm: mesh_path(c.text("FileName")),
                mesh: None,
                transform,
                materials: c.list("Materials").map(material).collect(),
                aabb,
                animated: animated
                    || c.child("Animations")
                        .is_some_and(|a| !a.children.is_empty()),
                bone_linked,
            });
        }
        for c in main.list("Files") {
            let file = c.text("FileName");
            if file.is_empty() {
                continue;
            }
            model.props.push(Prop {
                name: c.text("Name").to_string(),
                cfg: normalize_path(file).into_owned(),
                transform: transformer(c).0,
            });
        }
        for c in main.list("Decals") {
            let tex = c.vec4("TexCoords").unwrap_or([0.0, 0.0, 1.0, 1.0]);
            model.decals.push(Decal {
                transform: transformer(c).0,
                extents: c.vec3("Extents").unwrap_or_default(),
                uv_rect: tex,
                kind: c.text("Type").to_string(),
                material: c.list("Materials").next().map(material),
            });
        }
        for c in main.list("Particles") {
            model.particles.push(Particle {
                file: normalize_path(c.text("FileName")).into_owned(),
                transform: transformer(c).0,
            });
        }
        Ok(model)
    }

    /// Reads `cfg_path` from the install, loads every part's rdm and checks
    /// that referenced textures and prop cfgs exist. Missing files land in
    /// [`Model::problems`] and their fields become `None`.
    pub fn load(install: &Install, cfg_path: &str) -> Result<Model> {
        // BRP is the baked prop cache. The matching PRP keeps its full
        // source mesh and materials, which the editor can render directly.
        let prop = cfg_path
            .strip_suffix(".brp")
            .map(|stem| format!("{stem}.prp"));
        let source = prop
            .as_deref()
            .filter(|p| install.exists(p))
            .unwrap_or(cfg_path);
        let bytes = install.read(source)?;
        let mut model = Model::parse(&bytes).with_context(|| cfg_path.to_string())?;
        model.path = normalize_path(cfg_path).into_owned();
        let mut problems = Vec::new();
        // Shipped files and a mounted mod's: mods add models of their own.
        let exists = |p: &str| install.exists(p);
        for part in &mut model.parts {
            if !exists(&part.rdm) {
                problems.push(format!("{}: no mesh {}", part.name, part.rdm));
                continue;
            }
            match install.read(&part.rdm).and_then(|b| Rdm::parse(&b)) {
                Ok(mesh) => part.mesh = Some(mesh),
                Err(e) => problems.push(format!("{}: {e:#}", part.rdm)),
            }
        }
        // PRP sources have no cfg MeshCenter/MeshExtent. Placement tools
        // still need their footprint, in the same frame as the renderer.
        if model.mesh_extent == [0.; 3] {
            let mut bounds: Option<([f32; 3], [f32; 3])> = None;
            for part in &model.parts {
                for p in part.mesh.iter().flat_map(|m| &m.positions) {
                    let p = part.transform.apply(*p);
                    bounds = Some(match bounds {
                        None => (p, p),
                        Some((lo, hi)) => (
                            std::array::from_fn(|i| lo[i].min(p[i])),
                            std::array::from_fn(|i| hi[i].max(p[i])),
                        ),
                    });
                }
            }
            if let Some((lo, hi)) = bounds {
                model.mesh_center = std::array::from_fn(|i| (lo[i] + hi[i]) / 2.);
                model.mesh_extent = std::array::from_fn(|i| (hi[i] - lo[i]) / 2.);
            }
        }
        let materials = model
            .parts
            .iter_mut()
            .flat_map(|p| p.materials.iter_mut())
            .chain(model.decals.iter_mut().filter_map(|d| d.material.as_mut()));
        for m in materials {
            for tex in [&mut m.diffuse, &mut m.normal, &mut m.metallic] {
                if let Some(p) = tex.take_if(|p| !exists(p)) {
                    problems.push(format!("{}: no texture {p}", m.name));
                }
            }
        }
        for p in &model.props {
            if !exists(&p.cfg) {
                problems.push(format!("{}: no prop cfg {}", p.name, p.cfg));
            }
        }
        model.problems = problems;
        Ok(model)
    }
}

// Placement and flags from a `<Transformer>` list. Several
// ORIENTATION_TRANSFORMs compose in file order (outer first); the rest
// (visibility, color, day/night, damage, ...) are state-driven and ignored.
fn transformer(c: &Node) -> (Transform, bool, bool) {
    let mut t = Transform::IDENTITY;
    let (mut animated, mut bone) = (false, false);
    for x in c.list("Transformer") {
        match x.config_type() {
            Some("ORIENTATION_TRANSFORM") => {
                let s = x.f32("Scale").unwrap_or(1.0);
                let n = x.vec3("NonUniformScale").unwrap_or([1.0; 3]);
                let local = Transform {
                    position: x.vec3("Position").unwrap_or_default(),
                    rotation: x.vec4("Rotation").unwrap_or([0.0, 0.0, 0.0, 1.0]),
                    scale: n.map(|v| v * s),
                };
                t = t.then(&local);
            }
            Some("ANIMATION_TRANSFORM") => animated = true,
            Some("BONELINK_TRANSFORM") => bone = true,
            _ => {}
        }
    }
    (t, animated, bone)
}

fn material(c: &Node) -> Material {
    let shader = c.f32("ShaderID").unwrap_or(8.0) as u32;
    let on = |flag: &str| c.text(flag) != "0";
    let tex = |flag: &str, field: &str| {
        let src = match (field, c.text(field)) {
            ("cModelDiffTex", "") => c.text("cPropDiffuseTex"),
            ("cModelNormalTex", "") => c.text("cPropNormalTex"),
            _ => c.text(field),
        };
        (on(flag) && !src.is_empty()).then(|| texture_path(src))
    };
    let alpha = match shader {
        21..=23 => Alpha::Cutout(c.f32("cShadowAlphaRef").unwrap_or(0.5)),
        12 | 19 => Alpha::Blend(c.f32("cOpacity").unwrap_or(1.0)),
        1 | 2 | 6 | 26 => Alpha::Decal,
        4 | 5 => Alpha::Hidden,
        _ => Alpha::Opaque,
    };
    Material {
        name: c.text("Name").to_string(),
        shader,
        vertex_format: c.text("VertexFormat").to_string(),
        diffuse: tex("DIFFUSE_ENABLED", "cModelDiffTex"),
        normal: tex("NORMAL_ENABLED", "cModelNormalTex"),
        metallic: tex("METALLIC_TEX_ENABLED", "cModelMetallicTex"),
        diffuse_color: c.vec3_rgb("cDiffuseColor").unwrap_or([1.0; 3]),
        double_sided: c.text("DOUBLE_SIDED") == "1",
        terrain_tint: if c.text("cUseTerrainTinting") == "1" {
            c.f32("cTerrainTintIntensity")
                .unwrap_or(1.0)
                .clamp(0.0, 1.0)
        } else {
            0.0
        },
        alpha,
    }
}

/// Shipped mesh for a cfg `FileName`: the exporter's `.fbx` (or `.max`)
/// becomes `.rdm` next to it.
pub fn mesh_path(source: &str) -> String {
    let p = normalize_path(source);
    match p.rsplit_once('.') {
        Some((stem, _)) if !stem.ends_with('/') => format!("{stem}.rdm"),
        _ => p.into_owned(),
    }
}

/// Shipped texture for a cfg texture field: `maps/x_diff.psd` (or `.png`,
/// `.tga`) ships as `maps/x_diff_0.dds`, the `_0` being the top mip chain.
pub fn texture_path(source: &str) -> String {
    let p = normalize_path(source);
    match p.rsplit_once('.') {
        Some((_, "dds")) => p.into_owned(),
        Some((stem, _)) => format!("{stem}_0.dds"),
        None => format!("{p}_0.dds"),
    }
}

// Cfg element tree. Text is entity-decoded and trimmed.
#[derive(Debug, Default)]
struct Node {
    name: String,
    text: String,
    children: Vec<Node>,
}

impl Node {
    // Never fails: unbalanced input closes at EOF, stray closers are
    // dropped. The game's own reader is just as lenient.
    fn parse(bytes: &[u8]) -> Node {
        let s = String::from_utf8_lossy(bytes);
        let mut stack = vec![Node::default()];
        let mut rest = &s[..];
        while let Some(lt) = rest.find('<') {
            let text = &rest[..lt];
            if !text.trim().is_empty() {
                let top = stack.last_mut().expect("root stays on the stack");
                top.text.push_str(&decode_entities(text.trim()));
            }
            let Some(gt) = rest[lt..].find('>') else {
                break;
            };
            let tag = &rest[lt + 1..lt + gt];
            rest = &rest[lt + gt + 1..];
            if tag.starts_with('/') {
                if stack.len() > 1 {
                    let done = stack.pop().expect("len checked");
                    stack.last_mut().expect("len checked").children.push(done);
                }
            } else if let Some(name) = tag.strip_suffix('/') {
                let node = Node {
                    name: name.to_string(),
                    ..Node::default()
                };
                stack.last_mut().expect("root").children.push(node);
            } else {
                stack.push(Node {
                    name: tag.to_string(),
                    ..Node::default()
                });
            }
        }
        while stack.len() > 1 {
            let done = stack.pop().expect("len checked");
            stack.last_mut().expect("len checked").children.push(done);
        }
        stack.pop().expect("root")
    }

    fn child(&self, name: &str) -> Option<&Node> {
        self.children.iter().find(|c| c.name == name)
    }

    fn configs(&self) -> impl Iterator<Item = &Node> {
        self.children.iter().filter(|c| c.name == "Config")
    }

    // `<name><Config>…</Config><Config>…</Config></name>`
    fn list(&self, name: &str) -> impl Iterator<Item = &Node> {
        self.child(name).into_iter().flat_map(Node::configs)
    }

    fn config_type(&self) -> Option<&str> {
        self.child("ConfigType").map(|c| c.text.as_str())
    }

    fn text(&self, name: &str) -> &str {
        self.child(name).map_or("", |c| c.text.as_str())
    }

    fn f32(&self, name: &str) -> Option<f32> {
        self.child(name)?.text.parse().ok()
    }

    fn fields<const N: usize>(&self, prefix: &str, axes: [&str; N]) -> Option<[f32; N]> {
        let mut out = [0.0; N];
        for (o, axis) in out.iter_mut().zip(axes) {
            *o = self.f32(&format!("{prefix}.{axis}"))?;
        }
        Some(out)
    }

    // `Prefix.x`, `Prefix.y`, `Prefix.z` siblings.
    fn vec3(&self, prefix: &str) -> Option<[f32; 3]> {
        self.fields(prefix, ["x", "y", "z"])
    }

    fn vec4(&self, prefix: &str) -> Option<[f32; 4]> {
        self.fields(prefix, ["x", "y", "z", "w"])
    }

    fn vec3_rgb(&self, prefix: &str) -> Option<[f32; 3]> {
        self.fields(prefix, ["r", "g", "b"])
    }

    // `<x>…</x><y>…</y><z>…</z>` children (AABBMin/AABBMax).
    fn xyz(&self) -> [f32; 3] {
        ["x", "y", "z"].map(|a| self.f32(a).unwrap_or(0.0))
    }
}

fn decode_entities(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vfs::FileId;

    #[test]
    fn parses_1800_plant_props() {
        let model = Model::parse(br#"<Prop><Type>Plant</><VertexFormat>P4h_N4b_G4b_B4b_T2h_C4b_C4b</><MeshFileName>data/tree_lod0.rdm</><Materials><Material><DIFFUSE_ENABLED>1</><cPropDiffuseTex>data/tree_diff.psd</><cAlphaRef>0.25</><DOUBLE_SIDED>1</></></></>"#).unwrap();
        assert_eq!(model.parts.len(), 1);
        assert_eq!(model.parts[0].rdm, "data/tree_lod0.rdm");
        let material = &model.parts[0].materials[0];
        assert_eq!(material.diffuse.as_deref(), Some("data/tree_diff_0.dds"));
        assert_eq!(material.alpha, Alpha::Cutout(0.25));
        assert!(material.double_sided);
    }

    const CFG: &str = "<Config><ConfigType>MAIN</ConfigType>\r\n\
<MeshCenter.x>0</MeshCenter.x><MeshCenter.y>1</MeshCenter.y><MeshCenter.z>0</MeshCenter.z>\r\n\
<MeshExtent.x>2</MeshExtent.x><MeshExtent.y>1</MeshExtent.y><MeshExtent.z>2</MeshExtent.z>\r\n\
<Models><Config><ConfigType>MODEL</ConfigType>\r\n\
<Transformer><Config><ConfigType>ORIENTATION_TRANSFORM</ConfigType>\
<Position.x>1</Position.x><Position.y>0</Position.y><Position.z>0</Position.z>\
<Rotation.x>0</Rotation.x><Rotation.y>0.7071068</Rotation.y><Rotation.z>0</Rotation.z><Rotation.w>0.7071068</Rotation.w>\
<Scale>2</Scale><NonUniformScale.x>1</NonUniformScale.x><NonUniformScale.y>1</NonUniformScale.y><NonUniformScale.z>1</NonUniformScale.z>\
</Config><Config><ConfigType>ANIMATION_TRANSFORM</ConfigType></Config></Transformer>\r\n\
<Materials><Config><ConfigType>MATERIAL</ConfigType><Name>&lt;Default Material&gt;</Name><ShaderID>22</ShaderID>\
<Common [ModelCommon]>Common [ModelCommon]</Common [ModelCommon]><DIFFUSE_ENABLED>1</DIFFUSE_ENABLED>\
<cModelDiffTex>Data\\Base\\maps\\leaf_diff.psd</cModelDiffTex><NORMAL_ENABLED>0</NORMAL_ENABLED>\
<cModelNormalTex>data/base/maps/leaf_norm.png</cModelNormalTex><DOUBLE_SIDED>1</DOUBLE_SIDED><cUseTerrainTinting>1</cUseTerrainTinting><cTerrainTintIntensity>0.8</cTerrainTintIntensity>\
<cShadowAlphaRef>0.4</cShadowAlphaRef><cDiffuseColor.r>1.2</cDiffuseColor.r><cDiffuseColor.g>1</cDiffuseColor.g>\
<cDiffuseColor.b>1</cDiffuseColor.b></></Materials>\r\n\
<Name>leaf</Name><FileName>data/base/rdm/leaf_lod0.fbx</FileName>\
<AABBMin><x>-1</x><y>0</y><z>-1</z></AABBMin><AABBMax><x>1</x><y>2</y><z>1</z></AABBMax>\
</Config></Models>\r\n\
<Files><Config><ConfigType>FILE</ConfigType><Name>bucket</Name>\
<FileName>data/base/props/bucket.cfg</FileName></Config></Files>\r\n\
</Config>";

    #[test]
    fn parses_cfg_dialect() {
        let m = Model::parse(CFG.as_bytes()).unwrap();
        assert_eq!(
            (m.mesh_center, m.mesh_extent),
            ([0.0, 1.0, 0.0], [2.0, 1.0, 2.0])
        );
        let p = &m.parts[0];
        assert_eq!(
            (p.name.as_str(), p.rdm.as_str()),
            ("leaf", "data/base/rdm/leaf_lod0.rdm")
        );
        assert!(p.animated && !p.bone_linked);
        assert_eq!(p.aabb, Some(([-1.0, 0.0, -1.0], [1.0, 2.0, 1.0])));
        let mat = &p.materials[0];
        assert_eq!(mat.name, "<Default Material>");
        assert_eq!(
            mat.diffuse.as_deref(),
            Some("data/base/maps/leaf_diff_0.dds")
        );
        assert_eq!(mat.normal, None, "NORMAL_ENABLED 0 drops the map");
        assert_eq!(mat.alpha, Alpha::Cutout(0.4));
        assert!(mat.double_sided);
        assert_eq!(mat.terrain_tint, 0.8);
        assert_eq!(mat.diffuse_color, [1.2, 1.0, 1.0]);
        assert_eq!(m.props[0].cfg, "data/base/props/bucket.cfg");
        assert_eq!(m.props[0].transform, Transform::IDENTITY);

        // 90 degrees about +Y, scale 2, then +1 on X: +X maps to -Z.
        let t = p.transform;
        let q = t.apply([1.0, 0.0, 0.0]);
        assert!(
            (q[0] - 1.0).abs() < 1e-5 && (q[2] + 2.0).abs() < 1e-5,
            "{q:?}"
        );
        let mx = t.matrix();
        let by_matrix = [0, 1, 2].map(|r| mx[r] + mx[12 + r]);
        assert!((0..3).all(|k| (by_matrix[k] - q[k]).abs() < 1e-5));
    }

    #[test]
    fn source_paths_map_to_shipped_names() {
        assert_eq!(
            mesh_path("Data\\Base\\x\\rdm\\Tree_07.FBX"),
            "data/base/x/rdm/tree_07.rdm"
        );
        assert_eq!(
            texture_path("data/a/maps/b_diff.psd"),
            "data/a/maps/b_diff_0.dds"
        );
        assert_eq!(
            texture_path("data/engine/default_model_normal.png"),
            "data/engine/default_model_normal_0.dds"
        );
        assert_eq!(texture_path("data/a/x_0.dds"), "data/a/x_0.dds");
    }

    fn install() -> Option<Install> {
        let Ok(dir) = std::env::var("ANNO117_DIR") else {
            eprintln!("skip: ANNO117_DIR unset");
            return None;
        };
        Some(Install::open(dir).unwrap())
    }

    // Every loaded mesh: indices in range, unit normals, groups pointing at
    // cfg materials, and the rdm bounds inside the part's cfg AABB.
    fn check_parts(m: &Model) {
        for p in &m.parts {
            let mesh = p
                .mesh
                .as_ref()
                .unwrap_or_else(|| panic!("{}: {:?}", p.rdm, m.problems));
            let n = mesh.positions.len() as u32;
            assert!(mesh.indices.iter().all(|&i| i < n));
            for v in &mesh.normals {
                assert!((v.iter().map(|c| c * c).sum::<f32>().sqrt() - 1.0).abs() < 1e-3);
            }
            for g in &mesh.groups {
                assert!(
                    (g.material as usize) < p.materials.len(),
                    "{}: group {g:?}",
                    p.rdm
                );
            }
            if let Some((lo, hi)) = p.aabb {
                let (mlo, mhi) = mesh.computed_bounds();
                for k in 0..3 {
                    // f16 positions: ~1e-3 relative.
                    let tol = 0.01 + 1e-3 * hi[k].abs().max(lo[k].abs());
                    assert!(
                        mlo[k] >= lo[k] - tol && mhi[k] <= hi[k] + tol,
                        "{}: {k}",
                        p.rdm
                    );
                }
            }
        }
    }

    // Union of every part's transformed mesh corners; the cfg's
    // MeshCenter/MeshExtent box must contain it.
    fn assert_inside_mesh_bounds(m: &Model, tol: f32) {
        let lo = [0, 1, 2].map(|k| m.mesh_center[k] - m.mesh_extent[k] - tol);
        let hi = [0, 1, 2].map(|k| m.mesh_center[k] + m.mesh_extent[k] + tol);
        for p in &m.parts {
            let (a, b) = p.mesh.as_ref().unwrap().computed_bounds();
            for corner in 0..8 {
                let c = [0, 1, 2].map(|k| if corner >> k & 1 == 0 { a[k] } else { b[k] });
                let w = p.transform.apply(c);
                assert!(
                    (0..3).all(|k| w[k] >= lo[k] && w[k] <= hi[k]),
                    "{}: {w:?} outside {lo:?}..{hi:?}",
                    p.rdm
                );
            }
        }
    }

    #[test]
    fn olive_tree() {
        let Some(install) = install() else { return };
        let m = Model::load(
            &install,
            "data/base/graphics/roman/environment/props/vegetation/trees/olive_trees/olive_tree_07.cfg",
        )
        .unwrap();
        assert!(m.problems.is_empty(), "{:?}", m.problems);
        assert_eq!(m.parts.len(), 1);
        let p = &m.parts[0];
        assert!(p.rdm.ends_with("olive_trees/rdm/tree_07.rdm"));
        let [bark, leaf] = &p.materials[..] else {
            panic!()
        };
        assert_eq!((bark.alpha, bark.double_sided), (Alpha::Cutout(0.3), false));
        assert_eq!((leaf.alpha, leaf.double_sided), (Alpha::Cutout(0.3), true));
        assert!(
            bark.diffuse
                .as_deref()
                .unwrap()
                .ends_with("modules/maps/tree_bark_diff_0.dds")
        );
        assert!(
            leaf.normal
                .as_deref()
                .unwrap()
                .ends_with("olive_leaf_01_norm_0.dds")
        );
        assert_eq!(bark.normal, None);
        assert_eq!(bark.diffuse_color, [1.2; 3]);
        // The rdm material slots line up with the cfg list by name.
        let mesh = p.mesh.as_ref().unwrap();
        assert_eq!(mesh.materials[1].name, leaf.name);
        check_parts(&m);
        // A single untransformed part: the cfg's mesh box is the rdm AABB.
        let (lo, hi) = mesh.bounds;
        for k in 0..3 {
            assert!(((lo[k] + hi[k]) / 2.0 - m.mesh_center[k]).abs() < 1e-3);
            assert!(((hi[k] - lo[k]) / 2.0 - m.mesh_extent[k]).abs() < 1e-3);
        }
    }

    #[test]
    fn rock() {
        let Some(install) = install() else { return };
        let m = Model::load(
            &install,
            "data/base/graphics/roman/environment/props/rocks/roman_rock_02/roman_rock_02_c.cfg",
        )
        .unwrap();
        assert!(m.problems.is_empty(), "{:?}", m.problems);
        assert!(!m.parts.is_empty());
        assert!(
            m.parts
                .iter()
                .flat_map(|p| &p.materials)
                .all(|m| m.alpha == Alpha::Opaque || m.alpha == Alpha::Hidden)
        );
        check_parts(&m);
        assert_inside_mesh_bounds(&m, 0.02);
    }

    #[test]
    fn building_with_parts_and_props() {
        let Some(install) = install() else { return };
        let m = Model::load(
            &install,
            "data/base/graphics/roman/buildings/institution/institution_01/institution_01.cfg",
        )
        .unwrap();
        assert!(m.problems.is_empty(), "{:?}", m.problems);
        assert!(m.parts.len() >= 5, "{}", m.parts.len());
        assert!(m.parts.iter().any(|p| p.transform != Transform::IDENTITY));
        let buckets: Vec<_> = m
            .props
            .iter()
            .filter(|p| p.name == "bucket_water_01")
            .collect();
        assert!(buckets.len() >= 4);
        assert_eq!(buckets[0].transform.position, [1.27158, 0.0, -1.711167]);
        check_parts(&m);
        assert_inside_mesh_bounds(&m, 0.05);
        // Nested props load the same way.
        let bucket = Model::load(&install, &buckets[0].cfg).unwrap();
        assert!(!bucket.parts.is_empty() && bucket.problems.is_empty());
    }

    // ANNO117_SWEEP=1: load every cfg, count parts, props and problem kinds.
    #[test]
    fn sweep_install() {
        if std::env::var_os("ANNO117_SWEEP").is_none() {
            return;
        }
        let Some(install) = install() else { return };
        let (mut cfgs, mut parts, mut meshes, mut props, mut decals) = (0, 0, 0, 0, 0);
        let mut failed = Vec::new();
        let mut problems = std::collections::BTreeMap::<&str, (usize, Vec<String>)>::new();
        let mut alpha = std::collections::BTreeMap::<String, usize>::new();
        for i in 0..install.file_count() {
            let path = install.file_path(FileId(i as u32));
            if !path.ends_with(".cfg") || !path.starts_with("data/") {
                continue;
            }
            let m = match Model::load(&install, path) {
                Ok(m) => m,
                Err(e) => {
                    failed.push(format!("{path}: {e:#}"));
                    continue;
                }
            };
            cfgs += 1;
            parts += m.parts.len();
            meshes += m.parts.iter().filter(|p| p.mesh.is_some()).count();
            props += m.props.len();
            decals += m.decals.len();
            for p in m.parts.iter().flat_map(|p| &p.materials) {
                let key = format!("{:?}", p.alpha)
                    .split('(')
                    .next()
                    .unwrap()
                    .to_string();
                *alpha.entry(key).or_default() += 1;
            }
            for p in &m.problems {
                let kind = if p.contains("no texture") {
                    "missing texture"
                } else if p.contains("no mesh") {
                    "missing mesh"
                } else if p.contains("no prop") {
                    "missing prop cfg"
                } else {
                    "bad rdm"
                };
                let e = problems.entry(kind).or_default();
                e.0 += 1;
                if e.1.len() < 3 {
                    e.1.push(format!("{path}: {p}"));
                }
            }
        }
        eprintln!(
            "{cfgs} cfgs, {parts} parts ({meshes} meshes loaded), {props} props, {decals} decals"
        );
        eprintln!("materials by alpha: {alpha:?}");
        for (kind, (n, examples)) in &problems {
            eprintln!("{n:6} {kind}");
            for e in examples {
                eprintln!("         {e}");
            }
        }
        for f in failed.iter().take(10) {
            eprintln!("FAIL {f}");
        }
        assert!(failed.is_empty(), "{} cfgs failed", failed.len());
    }
}
