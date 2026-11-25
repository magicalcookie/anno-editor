// Anno 2070's building and prop material (model.fxh) as a surface family,
// its leaf boards' family, and how a 2070 mesh joins them (`dress`).

use std::sync::{Arc, OnceLock};

use anno_formats::model::{Alpha, Material, Shader};
use gpui_editor_kit::gpu::{Families, Family, MeshData};
use gpui_editor_kit::texture::Image;

/// The family's name in its registry.
pub const NAME: &str = "a2070building";
/// The leaf family's (`leaves`).
pub const LEAVES: &str = "a2070leaves";

// Payload word 0's bits.
const GLOW: u32 = 1;
const DYE: u32 = 2;
const REFLECT: u32 = 4;
const RIPPLES: u32 = 8;
const HEIGHT: u32 = 16;
const FOAM: u32 = 32;
/// Where foam stands: half the most opaque the game draws it, a quarter.
// TODO: placements that alpha-blend (as `Material::Blend` batches
// do) would let foam draw at the game's own opacity instead of this cut.
const FOAM_CUT: f32 = 0.125;

const WGSL: &str = r#"
// model.fxh: RIPPLE_SIZE, RIPPLE_NORMAL_INTENSITY.
const a2070building_RIPPLE_SIZE: f32 = 0.3;
const a2070building_RIPPLE_NORMAL: f32 = 4.0;

fn a2070building_coverage(material: GpuDraw, c: Cover) -> f32 {
    // model_foamfall.fx: Foam0.a · Foam1.b · vertex alpha · 0.25.
    if (family_word(material, 0u) & 32u) != 0u { return c.albedo.a * c.albedo.b * 0.25; }
    return c.albedo.a;
}

// The row's owner colour: 0x01RRGGBB, sRGB, decoded; white for a row
// whose word is 0 (no owner).
fn a2070building_owner(word: u32) -> vec3<f32> {
    if (word >> 24u) == 0u { return vec3<f32>(1.0); }
    let c = vec3<f32>(f32((word >> 16u) & 255u), f32((word >> 8u) & 255u), f32(word & 255u)) / 255.0;
    return decode(c);
}

// The mesh's slot of map `which` (0 mask, 1 ripples, 2 height), counting
// only the maps it has, in that order (a2070_buildings.rs `dress`).
fn a2070building_slot(material: GpuDraw, flags: u32, which: u32) -> u32 {
    var k = 0u;
    if which >= 1u && (flags & 6u) != 0u { k += 1u; }
    if which >= 2u && (flags & 8u) != 0u { k += 1u; }
    return family_slot(material, k);
}

fn a2070building_surface(material: GpuDraw, p: LitPixel, t: LitTexels) -> Lit {
    let flags = family_word(material, 0u);
    let has_normal = (material.maps & 0xffffu) != 0u;
    if (flags & 32u) != 0u {
        // GammaToLinear(Foam0.r · Foam1.r) times the object's colour, lit
        // from ambient to sun by N·L and shadow: no glow, no specular.
        let foam = t.albedo.r * t.albedo.r;
        let c = pow(vec3<f32>(foam), vec3<f32>(2.2)) * a2070building_owner(p.word);
        return Lit(mapped(c, p.normal, p.dp, p.duv, t.normal, false, vec4<f32>(0.0), false), vec3<f32>(0.0));
    }
    var c = t.albedo.rgb * p.color;
    var normal = t.normal;
    var glow = 0.0;
    if (flags & 1u) != 0u && has_normal { glow = normal.b; }
    if (flags & 8u) != 0u && has_normal {
        // Two ripple samples from world XZ, their normals multiplied, added
        // to the map's normal four times over (ps_Model).
        let at = p.world.xz * a2070building_RIPPLE_SIZE;
        let d = array<vec2<f32>, 2>(p.dp[0].xz * a2070building_RIPPLE_SIZE, p.dp[1].xz * a2070building_RIPPLE_SIZE);
        let slot = a2070building_slot(material, flags, 1u);
        let r1 = family_texel(slot, at, d);
        let r2 = family_texel(slot, at * vec2<f32>(1.0, -1.0) + vec2<f32>(0.37, 0.61), d);
        let ripple = (2.0 * r1.rgb - 1.0) * (2.0 * r2.rgb - 1.0);
        var n = vec3<f32>(2.0 * normal.rg - 1.0, 0.0);
        n.z = sqrt(max(0.0, 1.0 - dot(n.xy, n.xy)));
        n = normalize(vec3<f32>(n.xy + ripple.xy * a2070building_RIPPLE_NORMAL, n.z));
        normal = vec4<f32>(n.xy * 0.5 + 0.5, normal.b, normal.a);
    }
    if (flags & 4u) != 0u {
        // ps_Model's environment mapping, before the dye: the mask's red
        // blends the albedo toward the scene's cube along the reflected
        // view times the albedo ×1.5, both gamma-encoded as 2070 keeps
        // them (the kit's cube is linear).
        let m = family_texel(a2070building_slot(material, flags, 0u), p.uv, p.duv).r;
        let bent = mapped(c, p.normal, p.dp, p.duv, normal, has_normal, vec4<f32>(0.0), false).normal;
        let seen = environment(reflect(-toward_viewer(p.world), bent));
        let g = pow(max(c, vec3<f32>(1e-4)), vec3<f32>(1.0 / 2.2));
        let e = pow(max(seen, vec3<f32>(1e-4)), vec3<f32>(1.0 / 2.2));
        c = pow(max(mix(g, e * g * 1.5, m), vec3<f32>(1e-4)), vec3<f32>(2.2));
    }
    let owner = a2070building_owner(p.word);
    if (flags & 2u) != 0u {
        // AddDyeColor: up to a half the mask blends toward the colour's
        // grey times the owner's; above it, it glows.
        let m = family_texel(a2070building_slot(material, flags, 0u), p.uv, p.duv).g;
        glow += clamp((m - 0.5) * 2.0, 0.0, 1.0);
        let lum = dot(c, vec3<f32>(0.333));
        c = mix(c, lum * owner, clamp(m * 2.0, 0.0, 1.0));
    }
    // Without a dye mask ps_Model multiplies the albedo by the instance
    // colour (model.fxh:951). Players' buildings show their colour only
    // where masked, so the engine is taken to hand such instances white;
    // an estimate, not traced in Anno5.exe.
    glow = clamp(glow, 0.0, 1.0);
    let s = mapped(c * (1.0 - glow), p.normal, p.dp, p.duv, normal, has_normal, vec4<f32>(0.0), false);
    return Lit(s, c * glow * lights.glow.w);
}
"#;

/// The family, as its registry takes it: the normal map's blue glows, the
/// mask's green dyes by the row's owner colour (the instance word) and glows
/// above a half, wind ripples bend the normal, and the height map rides
/// along.
///
/// Lighting: 2070 lights a pixel `c · lerp(Lit, GLOW_BASE, glow)`
/// (deferredshading.fx:105-114), so the surface keeps `c · (1 − glow)` for
/// the scene's shade and gives off `c · glow` times the scene's glow
/// strength as emissive, as the kit splits a glowing surface. 2070's own
/// shade takes the strength back out and lerps toward GLOW_BASE itself;
/// under the kit's shade (previews, thumbnails) the model look's strength,
/// GLOW_BASE, makes it eight times the albedo.
///
/// Under `EnvironmentEnabled` the mask's red blends the albedo toward the
/// scene's environment cube (`environment`), times the albedo ×1.5.
///
/// Waterfalls' foam (model_foamfall.fx, `ModelFx::foam`) is the family's
/// too: grey from the foam texture's red, squared as its two scrolling
/// samples multiply, with the texture's alpha times blue as opacity at a
/// quarter (the vertex alpha, unread, taken as 1).
///
/// Not modelled:
/// - foam translucency: the game blends the foam over what is behind at up
///   to a quarter's opacity; placements here only cut, so it draws solid
///   where its opacity is at least half that (`FOAM_CUT`).
/// - foam motion: the two samples scroll down the fall by the vertex colour's
///   green and blue times the clock; with no clock they coincide.
/// - parallax: 2070 displaces by the height map with DX11 tessellation, a
///   vertex stage a family can't have. The map is held (slot), unread.
/// - ripple motion: the game scrolls the ripple UVs by its timer; families
///   get no clock, so they stand still.
pub fn family() -> Family {
    Family {
        name: NAME.into(),
        wgsl: WGSL.into(),
        light: false,
        vertex: false,
        // The coverage reads the albedo alone.
        coverage_slots: 0,
        // It gives off light, which impostors don't bake.
        bake: false,
    }
}

/// A tree's leaves as the game draws them (instancing_bbcluster.fx): each
/// board a square turned to the camera's right and up about its centre
/// (`BillPos = MtxTransform · (Position · Scale)`), shaded and cut out as
/// the kit's `lit` family does. Its meshes come from formats' `leaves`
/// (`dress` keeps one quad per board) with each vertex's part centre the
/// board's.
pub fn leaves() -> Family {
    Family {
        name: LEAVES.into(),
        wgsl: LEAVES_WGSL.into(),
        light: false,
        vertex: true,
        coverage_slots: 0,
        // A vertex stage bakes into no impostor.
        bake: false,
    }
}

const LEAVES_WGSL: &str = r#"
fn a2070leaves_coverage(material: GpuDraw, c: Cover) -> f32 {
    return lit_coverage(material, c);
}

fn a2070leaves_surface(material: GpuDraw, p: LitPixel, t: LitTexels) -> Lit {
    return lit_surface(material, p, t);
}

// The board's centre placed by the row, and the vertex's offset from it
// (grown or shrunk by thinning) laid along the camera's right and up, at
// the row's scale. The camera's axes, not the way to the board: the game's
// MtxTransform is the view's rotation.
fn a2070leaves_vertex(material: GpuDraw, v: FamilyVertex) -> vec3<f32> {
    if v.part.w < 0.0 { return v.world; }
    let centre = (v.row * vec4<f32>(v.part.xyz, 1.0)).xyz;
    var forward = -globals.eye.xyz;
    if globals.eye.w > 0.5 { forward = normalize(globals.anchor.xyz - globals.eye.xyz); }
    var right = cross(vec3<f32>(0.0, 1.0, 0.0), forward);
    if dot(right, right) < 1e-8 { right = vec3<f32>(1.0, 0.0, 0.0); }
    right = normalize(right);
    let up = cross(forward, right);
    let o = (v.local - v.part.xyz) * length(v.row[0].xyz);
    return centre + right * o.x + up * o.y;
}
"#;

/// 2070's model families in registry order: buildings, then leaves. Every
/// set 2070 draws with lists them so, so the ids its meshes carry hold in
/// each.
pub fn surfaces() -> Vec<Family> {
    vec![family(), leaves()]
}

/// The registry 2070's renderers draw its models with: the kit's family
/// and 2070's (`surfaces`). One per process, so every renderer shares its
/// pipelines.
pub fn families() -> Arc<Families> {
    static FAMILIES: OnceLock<Arc<Families>> = OnceLock::new();
    FAMILIES
        .get_or_init(|| Families::new(surfaces()).expect("2070's model families register"))
        .clone()
}

/// A leaf cluster's mesh as the leaf family draws it: of each board's two
/// crossed quads (eight vertices in a row, the one across X first, as
/// formats' `leaves` lays them out) the first alone, each vertex's part the
/// board's centre. The island's loader ranks the boards for thinning after
/// this, keeping those centres; until then every board draws.
fn billboards(data: &mut MeshData) {
    let n = data.positions.len();
    if n == 0 || n % 8 != 0 {
        return;
    }
    let mut parts = Vec::with_capacity(n);
    for board in data.positions.chunks(8) {
        let c = board
            .iter()
            .fold([0.; 3], |m, p| [0, 1, 2].map(|a| m[a] + p[a] / 8.));
        parts.extend([[c[0], c[1], c[2], 0.]; 8]);
    }
    let tris = data.indices.as_chunks::<3>().0;
    data.indices = (tris.iter())
        .filter(|t| t.iter().all(|&i| i % 8 < 4))
        .flatten()
        .copied()
        .collect();
    data.thinning = Some(gpui_editor_kit::gpu::Thinning {
        parts,
        full: 1.,
        least: 1.,
    });
    data.family = families().id(LEAVES).expect("registered");
}

/// The owner colour `rgb` (sRGB bytes) as a row's instance word.
pub fn owner_word(rgb: [u8; 3]) -> u32 {
    1 << 24 | u32::from(rgb[0]) << 16 | u32::from(rgb[1]) << 8 | u32::from(rgb[2])
}

/// Puts `data`, a lit mesh of the 2070 `material`, in the family when the
/// material glows, dyes, ripples or has a height map, with its payload and
/// maps; `texture` reads one of the material's textures at the mesh's
/// size. Other meshes (cutout plants, the later games') stay the kit's.
pub fn dress(
    material: &Material,
    data: &mut MeshData,
    texture: &mut dyn FnMut(&str) -> Option<Image>,
) {
    if material.vertex_format == anno_formats::model::Material::BILLBOARDS {
        billboards(data);
        return;
    }
    let Shader::ModelFx(_, own) = &material.shader else {
        return;
    };
    if matches!(material.alpha, Alpha::Decal | Alpha::Hidden) {
        return;
    }
    let mut flags = 0;
    let mut slots = Vec::new();
    let mask = own.mask.as_deref().and_then(&mut *texture);
    if let Some(mask) = mask {
        flags |= if own.dye { DYE } else { 0 } | if own.reflect { REFLECT } else { 0 };
        if flags != 0 {
            slots.push(mask);
        }
    }
    if let Some(ripples) = own.ripples.as_deref().and_then(&mut *texture) {
        flags |= RIPPLES;
        slots.push(ripples);
    }
    if let Some(height) = own.height.as_deref().and_then(&mut *texture) {
        flags |= HEIGHT;
        slots.push(height);
    }
    if own.glow {
        flags |= GLOW;
    }
    if own.foam {
        // The foam shader reads the diffuse alone.
        (flags, slots) = (FOAM, Vec::new());
        data.alpha_ref = Some(FOAM_CUT);
        data.ignore_alpha = false;
    }
    if flags == 0 {
        return;
    }
    data.family = families().id(NAME).expect("registered");
    data.payload = vec![flags];
    data.slots = slots;
    // The family gives off its own light; the reflection mask is its slot.
    data.emissive = None;
    data.surface_map = None;
}

#[cfg(test)]
mod tests {
    use super::*;
    use anno_formats::model::{Model, ModelFx};
    use gpui_editor_kit::camera::{Camera, Extent};
    use gpui_editor_kit::gpu::{Light, Material as Kind, Mesh, Offscreen, Piece, Renderer, Scene};
    use gpui_editor_kit::math;
    use gpui_editor_kit::texture::Texture;

    /// A 2070 material with `own`'s switches, opaque.
    fn material(own: ModelFx) -> Material {
        let mut m = Model::parse(
            b"<m_Config><m_Models><m_Config><m_ConfigType>MODEL</m_ConfigType>\
<m_FileName>a.rdm</m_FileName><m_Materials><m_Config><m_ShaderIndex>10</m_ShaderIndex>\
<m_ConfigType>MATERIAL</m_ConfigType></m_Config></m_Materials></m_Config></m_Models></m_Config>",
        )
        .unwrap()
        .parts
        .remove(0)
        .materials
        .remove(0);
        m.shader = Shader::ModelFx(10, own);
        m
    }
    fn solid(rgba: [u8; 4]) -> Image {
        Image::from(Texture::solid(rgba))
    }

    /// A unit square on the ground in 32 × 32 cells; grey, with a flat
    /// normal map whose blue is `glow`, dressed as `material` with its
    /// mask's green `dye`.
    fn square(material: &Material, glow: u8, dye: u8) -> MeshData {
        const N: u32 = 32;
        let mut data = MeshData {
            material: Kind::Lit,
            texture: Some(solid([128, 128, 128, 255])),
            normal_map: Some(solid([128, 128, glow, 255])),
            ignore_alpha: true,
            ..Default::default()
        };
        for j in 0..=N {
            for i in 0..=N {
                let (u, v) = (i as f32 / N as f32, j as f32 / N as f32);
                data.positions.push([u - 0.5, 0., v - 0.5]);
                data.uvs.push([u, v]);
                data.normals.push([0., 1., 0.]);
            }
        }
        for j in 0..N {
            for i in 0..N {
                let a = j * (N + 1) + i;
                data.indices
                    .extend([a, a + N + 1, a + 1, a + 1, a + N + 1, a + N + 2]);
            }
        }
        dress(material, &mut data, &mut |t: &str| {
            (t == "mask").then(|| solid([0, dye, 0, 255]))
        });
        data
    }

    // Needs a Vulkan device: a leaf cluster's board, two crossed quads as
    // formats' `leaves` builds them, joins the leaf family with one quad
    // kept and its vertices' parts at the board's centre, and draws turned
    // to the camera: seen from straight above it covers a full square,
    // where the kit's family shows the crossed quads edge on.
    #[test]
    fn a_leaf_board_turns_to_face_the_camera() {
        let mut leaf = material(ModelFx {
            glow: false,
            dye: false,
            mask: None,
            reflect: false,
            ripples: None,
            height: None,
            foam: false,
        });
        leaf.vertex_format = anno_formats::model::Material::BILLBOARDS.into();
        let mut data = MeshData {
            material: Kind::Lit,
            texture: Some(solid([40, 220, 60, 255])),
            ..Default::default()
        };
        for across in [[0.5, 0., 0.], [0., 0., 0.5]] {
            let first = data.positions.len() as u32;
            for (side, up, uv) in [
                (-1., 1., [0., 0.]),
                (1., 1., [1., 0.]),
                (1., -1., [1., 1.]),
                (-1., -1., [0., 1.]),
            ] {
                data.positions
                    .push([across[0] * side, 0.5 * up, across[2] * side]);
                data.normals.push([0., 1., 0.]);
                data.uvs.push(uv);
            }
            data.indices
                .extend([0, 1, 2, 0, 2, 3, 0, 2, 1, 0, 3, 2].map(|i| first + i));
        }
        let plain = data.clone();
        dress(&leaf, &mut data, &mut |_| None);
        assert_eq!(Some(data.family), families().id(LEAVES));
        assert_eq!(data.indices.len(), 12, "one quad kept, both ways round");
        let parts = &data.thinning.as_ref().unwrap().parts;
        assert!(parts.iter().all(|p| p[..3] == [0.; 3]));
        // Straight down, nearly.
        let camera = Camera {
            tilt: 1e-3,
            ..Default::default()
        };
        let world = Extent {
            center: [0.; 3],
            size: 6.,
        };
        let green = |pixels: &[u8]| pixels.chunks(4).filter(|p| p[1] > 120 && p[0] < 90).count();
        let draw = |mesh: &MeshData| {
            let renderer = Renderer::with_families(families());
            renderer.set_scene(&Scene {
                light: Light {
                    sun_color: [0.; 3],
                    sky: [1.; 3],
                    ground: [1.; 3],
                    ..Default::default()
                },
                ..Default::default()
            });
            renderer.set_pieces(vec![Piece {
                levels: vec![(Arc::new(Mesh::new(mesh.clone()).unwrap()), 0.)],
                impostor: None,
                pose: None,
            }]);
            let mut m = math::translate([0., 1., 0.]);
            (m[0], m[5], m[10]) = (2., 2., 2.);
            renderer.set_placements(&[0], &[m]);
            let mut target = Offscreen::new(96, 96).unwrap();
            green(&target.render(&renderer, camera, world).unwrap())
        };
        let edge_on = draw(&plain);
        let facing = draw(&data);
        eprintln!("edge on {edge_on} pixels; facing {facing}");
        assert!(edge_on < 200, "edge on: {edge_on}");
        assert!(facing > 700, "facing: {facing}");
    }

    // Needs a Vulkan device: waterfall foam (model_foamfall.fx) draws grey
    // from its texture's red, lit, never the texture's own colour: a texel
    // of full red and blue (the shipped foam's magenta) shows white under
    // white ambient. Where alpha times blue leaves the game's opacity under
    // half its quarter, it is cut away.
    #[test]
    fn waterfall_foam_draws_grey_from_red_and_cuts_by_alpha_times_blue() {
        let foam = material(ModelFx {
            glow: false,
            dye: false,
            mask: None,
            reflect: false,
            ripples: None,
            height: None,
            foam: true,
        });
        let mesh = |rgba: [u8; 4]| {
            let mut data = square(&foam, 0, 0);
            data.texture = Some(solid(rgba));
            data
        };
        let dense = mesh([255, 0, 255, 255]);
        assert_eq!(dense.payload, [FOAM]);
        assert_eq!(dense.alpha_ref, Some(FOAM_CUT));
        assert!(dense.slots.is_empty() && !dense.ignore_alpha);
        let thin = mesh([255, 0, 64, 255]);
        let world = Extent {
            center: [0.; 3],
            size: 2.,
        };
        let draw = |data: &MeshData| {
            let renderer = Renderer::with_families(families());
            renderer.set_scene(&Scene {
                light: Light {
                    sun_color: [0.; 3],
                    sky: [1.; 3],
                    ground: [1.; 3],
                    ..Default::default()
                },
                ..Default::default()
            });
            renderer.set_pieces(vec![Piece {
                levels: vec![(Arc::new(Mesh::new(data.clone()).unwrap()), 0.)],
                impostor: None,
                pose: None,
            }]);
            renderer.set_placements(&[0], &[math::translate([0.; 3])]);
            let mut target = Offscreen::new(64, 64).unwrap();
            let pixels = target.render(&renderer, Camera::default(), world).unwrap();
            let i = (32 * 64 + 32) * 4;
            [pixels[i], pixels[i + 1], pixels[i + 2]]
        };
        let [r, g, b] = draw(&dense);
        assert!(r > 200 && g > 200 && b > 200, "{:?}", [r, g, b]);
        // Thin foam shows what's behind, as a clear texel does.
        let behind = draw(&mesh([255, 0, 255, 0]));
        assert_eq!(draw(&thin), behind);
        assert_ne!(behind, [r, g, b]);
    }

    // Needs a Vulkan device: a mask whose red is full reflects the scene's
    // environment cube as ps_Model does, `lerp(c, cube·c·1.5, red)` in
    // gamma. A cube of one colour whose gamma value times 1.5 is 1 gives
    // the albedo back exactly; a brighter one brightens it, a darker one
    // darkens it, and one without the mask's red stays as it was.
    #[test]
    fn a_reflecting_mask_reflects_the_environment_cube() {
        use gpui_editor_kit::gpu::Environment;
        let own = ModelFx {
            glow: false,
            dye: false,
            mask: Some("mask".into()),
            reflect: true,
            ripples: None,
            height: None,
            foam: false,
        };
        let mesh = |red: u8| {
            let mut data = square(&material(own.clone()), 0, 0);
            data.slots = vec![solid([red, 0, 0, 255])];
            data
        };
        assert_eq!(mesh(255).payload, [REFLECT]);
        let world = Extent {
            center: [0.; 3],
            size: 2.,
        };
        let camera = Camera::default();
        let draw = |red: u8, cube: Option<f32>| {
            let mut target = Offscreen::new(64, 64).unwrap();
            let renderer = Renderer::with_families(families());
            renderer.set_scene(&Scene {
                light: Light {
                    sun_color: [0.; 3],
                    sky: [1.; 3],
                    ground: [1.; 3],
                    ..Default::default()
                },
                environment: cube.map(|c| Environment::from_fn(2, |_| [c; 3])),
                ..Default::default()
            });
            renderer.set_pieces(vec![Piece {
                levels: vec![(Arc::new(Mesh::new(mesh(red)).unwrap()), 0.)],
                impostor: None,
                pose: None,
            }]);
            renderer.set_placements(&[0], &[math::translate([0.; 3])]);
            let pixels = target.render(&renderer, camera, world).unwrap();
            let i = (32 * 64 + 32) * 4;
            [pixels[i], pixels[i + 1], pixels[i + 2]]
        };
        // (2/3)^2.2: 1.5 times its gamma value is 1.
        let neutral = (2f32 / 3.).powf(2.2);
        let plain = draw(0, Some(0.9));
        let (even, bright, dark) = (
            draw(255, Some(neutral)),
            draw(255, Some(0.9)),
            draw(255, Some(0.1)),
        );
        let close = |a: [u8; 3], b: [u8; 3]| (0..3).all(|k| a[k].abs_diff(b[k]) <= 2);
        assert!(close(even, plain), "{even:?} {plain:?}");
        assert!(bright[0] > plain[0] + 20, "{bright:?} {plain:?}");
        assert!(dark[0] + 20 < plain[0], "{dark:?} {plain:?}");
        assert_eq!(draw(0, Some(0.1)), plain);
    }

    // Needs a Vulkan device: the building family registers, and draws. Read
    // back from rows of three meshes: an undyed one keeps its own colour whatever its row's owner;
    // a dyed one blends toward the colour's grey times the
    // owner's where its mask's green is a half; one whose normal map's blue
    // is full glows at eight times its albedo, far past the unlit grey.
    #[test]
    fn a_building_family_draws_dye_and_glow() {
        let families = families();
        let id = families.id(NAME).unwrap();
        let glow = ModelFx {
            glow: true,
            dye: false,
            mask: None,
            reflect: false,
            ripples: None,
            height: None,
            foam: false,
        };
        let plain = material(glow.clone());
        let dyed = material(ModelFx {
            dye: true,
            mask: Some("mask".into()),
            ..glow
        });
        let meshes = [
            square(&plain, 0, 0),
            square(&dyed, 0, 128),
            square(&plain, 255, 0),
        ];
        assert!(
            meshes
                .iter()
                .all(|m| m.family == id && m.emissive.is_none())
        );
        assert_eq!(meshes[1].payload, [GLOW | DYE]);
        assert_eq!(meshes[1].slots.len(), 1);
        // (mesh, owner word): undyed without and with a red owner; dyed
        // with a blue owner and without; glowing.
        let red = owner_word([255, 0, 0]);
        let blue = owner_word([0, 0, 255]);
        let rows = [(0, 0), (0, red), (1, blue), (1, 0), (2, 0)];
        let at: Vec<math::Mat4> = (0..rows.len())
            .map(|r| math::translate([r as f32 * 1.5 - 3., 0., 0.]))
            .collect();
        let world = Extent {
            center: [0.; 3],
            size: 8.,
        };
        let camera = Camera::default();
        let mut target = Offscreen::new(320, 240).unwrap();
        let renderer = Renderer::with_families(families.clone());
        // Ambient white from every side: the albedo, unlit; glowing at
        // the model look's strength, as previews draw it.
        renderer.set_scene(&Scene {
            light: Light {
                sun_color: [0.; 3],
                sky: [1.; 3],
                ground: [1.; 3],
                ..Default::default()
            },
            glow: gpui_editor_kit::gpu::Glow {
                strength: 8.,
                ..Default::default()
            },
            ..Default::default()
        });
        renderer.set_pieces(
            meshes
                .iter()
                .map(|m| Piece {
                    levels: vec![(Arc::new(Mesh::new(m.clone()).unwrap()), 0.)],
                    impostor: None,
                    pose: None,
                })
                .collect(),
        );
        let piece_of: Vec<u32> = rows.iter().map(|&(m, _)| m).collect();
        renderer.set_placements(&piece_of, &at);
        renderer.set_instance_words(0, &rows.map(|(_, w)| w));
        let pixels = target.render(&renderer, camera, world).unwrap();
        let px = |r: usize| {
            let p = camera.project(math::translation(&at[r]), target.bounds(), world);
            let (x, y) = (f32::from(p.x) as usize, f32::from(p.y) as usize);
            let i = (y * 320 + x) * 4;
            [pixels[i], pixels[i + 1], pixels[i + 2]]
        };
        let [grey, owned, blue, undyed, glowing] = [0, 1, 2, 3, 4].map(px);
        eprintln!("{grey:?} {owned:?} {blue:?} {undyed:?} {glowing:?}");
        let close = |a: [u8; 3], b: [u8; 3]| (0..3).all(|k| a[k].abs_diff(b[k]) <= 6);
        assert!(grey[0] > 60 && close(grey, [grey[0]; 3]), "{grey:?}");
        assert!(
            close(owned, grey),
            "an owner tints no unmasked mesh: {owned:?}"
        );
        assert!(blue[2] > 30 && blue[0] < 12 && blue[1] < 12, "{blue:?}");
        assert!(close(undyed, grey), "{undyed:?} against {grey:?}");
        assert!(
            (0..3).all(|k| glowing[k] > grey[k].saturating_add(80)),
            "{glowing:?}"
        );
    }
}
