// Owns 2070's light: its mood profiles (`ambientmap.xml`), which one lights
// an island, the `Ambient` and `Sky` the editor lights an island by, and the
// atmospheric scattering the game fogs and paints its sky's light with.
//
// 2070 runs one light profile per zone of the world: a zone per placed
// island and four global ones. An island's zone takes its world entry's
// `LightProfile`, or else the `GlobalAreas` entry `NorthIsland`
// (`Underwater_Island_North` under water); each global zone takes the
// world's `WorldLightProfiles` override, or else its area's own setting
// (CGRIDClipper's zone setup, Anno5.exe 0x140B7AB30). The editor shows one
// island, so its zone's profile is all it needs.
// Engine addresses here are Anno5.exe build 11514956 (Steam).

use std::sync::{Arc, OnceLock};

use anno_core::ambient::{Ambient, Baked, SunMoon};
use anno_formats::isd::{Doc, NodeId, Value};
use anno_formats::vfs::{FileId, Install};
use anno_formats::xml;
use anyhow::{Context, Result, bail};
use gpui_editor_kit::gpu::{Curve, Families, HOST_WORDS, Lighting, SkyMap};

/// The mood profiles, and the zone defaults islands fall back to.
pub const AMBIENT_MAP: &str = "data/config/engine/ambientmap.xml";

/// The camera's clip planes (`camera.xml` `m_FrontClip`, `m_BackClip`):
/// scattering runs from a share of the way between them to the far one
/// (Anno5.exe 0x14084D880, which builds `cAtmosphericData1`).
pub const NEAR: f32 = 1.5;
pub const FAR: f32 = 350.;
/// How far the game's camera backs off its anchor (`m_MaxDistance`). The
/// editor's backs off further, so the scattering measures from an eye held
/// this close: a whole island seen from 600 m would otherwise sink into
/// the haze a player never sees it under.
pub const MAX_DISTANCE: f32 = 165.;
/// What glow lerps the light toward (deferredshading.fx:44-48), without
/// `LOW_QUALITY_HDR_ENABLED`.
/// TODO: 4 under low-quality HDR; which one runs is a capture's
/// to say.
pub const GLOW_BASE: f32 = 8.;

/// One of a profile's three settings. 2070 lerps an island's zone across
/// them by its eco balance, mapped to 0..1 through a curve of the game's
/// balance tables (0x140520540, 0x140521310) and sent to the zone
/// (render command 241).
/// TODO: which end of that factor each setting is, and the
/// factor an unbuilt island starts at; the editor shows setting 0.
#[derive(Clone, Debug, PartialEq)]
pub struct Profile {
    /// `<name>:<setting>`, as the ambient list names it.
    pub name: String,
    pub light: Light,
    pub water: Water,
    pub atmosphere: Atmosphere,
    pub wind: [f32; 4],
    pub current: [f32; 4],
    pub blur_strength: f32,
    /// The way the sun's light travels (y below 0).
    pub light_direction: [f32; 3],
    pub cam_y_rot_offset: f32,
}

/// The light colours, gamma-encoded, each scaled by its power when the
/// game hands the profile to the renderer (0x140B04030 multiplies all
/// seven).
#[derive(Clone, Debug, PartialEq)]
pub struct Light {
    /// `DiffuseColor`: the sun's light on the ground (`cLgtDiffuse`).
    pub diffuse: [f32; 3],
    pub diffuse_power: f32,
    /// `AmbientColor` (`cLgtAmbient`).
    pub ambient: [f32; 3],
    pub ambient_power: f32,
    /// `MoodColor`, with its alpha: a grade no shader the editor ports
    /// reads yet.
    pub mood: [f32; 4],
    pub mood_power: f32,
    pub inv_cloud_shadow: f32,
    pub sun_color_power: f32,
    pub horizon_color_power: f32,
    pub zenith_color_power: f32,
    pub cloud_color_power: f32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Water {
    pub wave_frequency: [f32; 2],
    pub wave_amplitude: [f32; 2],
    pub wave_velocity: [[f32; 2]; 2],
    pub choppiness: f32,
    pub above: WaterLight,
    pub under: WaterLight,
    pub under_dof_focus: f32,
}

/// How light dies in the water, seen from above or below.
#[derive(Clone, Debug, PartialEq)]
pub struct WaterLight {
    pub absorption: [f32; 3],
    pub light_depth: f32,
    pub view_depth: f32,
    pub view_depth_exponent: f32,
    pub scattering: [f32; 3],
}

/// `Atmosphere`: the scattering and the sky. Colours are gamma-encoded,
/// before their powers (`Light`).
#[derive(Clone, Debug, PartialEq)]
pub struct Atmosphere {
    pub scatter_start: f32,
    /// Radians from the sun.
    pub sun_inner_angle: f32,
    pub sun_outer_angle: f32,
    pub sun_falloff: f32,
    pub horizon_falloff: f32,
    pub distance_falloff: f32,
    pub cloud_density: f32,
    pub sun_color: [f32; 3],
    pub horizon_color: [f32; 3],
    pub zenith_color: [f32; 3],
    pub cloud_color: [f32; 3],
}

impl Profile {
    /// Toward the sun.
    pub fn toward_sun(&self) -> [f32; 3] {
        let d = self.light_direction;
        let len = (d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt().max(1e-6);
        d.map(|c| -c / len)
    }

    /// `cLgtDiffuse` and `cLgtAmbient`, gamma-encoded.
    pub fn sun(&self) -> [f32; 3] {
        self.light.diffuse.map(|c| c * self.light.diffuse_power)
    }

    pub fn ambient(&self) -> [f32; 3] {
        self.light.ambient.map(|c| c * self.light.ambient_power)
    }

    /// What the scattering reads of the profile (`cbScattering`).
    pub fn scatter(&self) -> Scatter {
        let a = &self.atmosphere;
        let l = &self.light;
        // The renderer spreads the two fall-offs a little per channel, red
        // falling off first (0x14084D880).
        let spread = |f: f32| [f, 0.9 * f, 0.75 * f];
        let (inner, outer) = (a.sun_inner_angle.cos(), a.sun_outer_angle.cos());
        let start = NEAR + a.scatter_start * (FAR - NEAR);
        Scatter {
            horizon: a.horizon_color.map(|c| c * l.horizon_color_power),
            zenith: a.zenith_color.map(|c| c * l.zenith_color_power),
            horizon_falloff: spread(a.horizon_falloff),
            distance_falloff: spread(a.distance_falloff),
            sun_falloff: a.sun_falloff,
            sun_outer: outer,
            sun_spread: 1. / (inner - outer),
            start,
            depth: 1. / (FAR - start - 5.).max(1e-4),
        }
    }
}

/// `cbScattering` as the renderer fills it for a profile (0x14084D880),
/// less what follows the camera: the horizon offset is the eye's height
/// over the far plane, worked out per view.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Scatter {
    /// `cHorizonColor`, `cZenithColor`: gamma-encoded.
    pub horizon: [f32; 3],
    pub zenith: [f32; 3],
    pub horizon_falloff: [f32; 3],
    pub distance_falloff: [f32; 3],
    pub sun_falloff: f32,
    /// `cAtmosphericData0.zw`: the cosine of the sun's outer angle, and
    /// one over the inner's less it.
    pub sun_outer: f32,
    pub sun_spread: f32,
    /// `cAtmosphericData1.x` and `.z`: where scattering starts, and one
    /// over how far (less 5 m) it runs to the far plane.
    pub start: f32,
    pub depth: f32,
}

impl Scatter {
    /// `ScatteringVS` (functions.fxh:352-378): extinction and inscattered
    /// light at `world` seen from `eye`, the sun toward `sun`. A colour
    /// fogs as `c·Fex + Fin` in gamma space.
    pub fn at(&self, eye: [f32; 3], world: [f32; 3], sun: [f32; 3]) -> ([f32; 3], [f32; 3]) {
        let d = [world[0] - eye[0], world[1] - eye[1], world[2] - eye[2]];
        let distance = (d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt().max(1e-6);
        let view = d.map(|c| c / distance);
        self.along(eye[1], view, distance, sun)
    }

    /// `at` for a view along unit `view` from an eye `height` up, out to
    /// `distance`.
    pub fn along(
        &self,
        height: f32,
        view: [f32; 3],
        distance: f32,
        sun: [f32; 3],
    ) -> ([f32; 3], [f32; 3]) {
        let saturate = |x: f32| x.clamp(0., 1.);
        let offset = height / FAR;
        let reciprocal = 1. / (offset + 1.);
        let sun_angle = view[0] * sun[0] + view[1] * sun[1] + view[2] * sun[2];
        let glow = saturate(saturate(sun_angle) - self.sun_outer) * self.sun_spread;
        let glow = glow.max(0.).powf(self.sun_falloff);
        let sun_angle = saturate(sun_angle * sun_angle * 0.3 + 0.7);
        let zenith = 1. - saturate((view[1] + offset) * reciprocal);
        let depth = saturate((distance - self.start) * self.depth);
        let sun_tint = [0.5, 0.4, 0.1];
        let mut extinction = [0.; 3];
        let mut inscatter = [0.; 3];
        for c in 0..3 {
            let toward = depth * zenith.max(0.).powf(self.horizon_falloff[c]);
            let scattering = sun_angle * lerp(self.zenith[c], self.horizon[c], toward)
                + sun_tint[c] * 0.7 * glow;
            let depth = depth.powf(self.distance_falloff[c]);
            extinction[c] = 1. - depth;
            inscatter[c] = depth * scattering;
        }
        (extinction, inscatter)
    }

    /// The scattering as WGSL's `scene_atmosphere` takes it, five vec4:
    /// the scene's fog writes these in, and the passes that fog themselves
    /// (the sea, the lakes) upload them, so all read one set.
    pub fn words(&self) -> [[f32; 4]; 5] {
        let rgb = |c: [f32; 3], w: f32| [c[0], c[1], c[2], w];
        [
            rgb(self.horizon, self.sun_falloff),
            rgb(self.zenith, self.sun_outer),
            rgb(self.horizon_falloff, self.sun_spread),
            rgb(self.distance_falloff, self.start),
            [self.depth, FAR, MAX_DISTANCE, 0.],
        ]
    }
}

/// 2070's scattering in WGSL (`ScatteringVS` and `ScatteringPS`,
/// functions.fxh:352-392), after the frame prelude: the scene's `fog`
/// calls it per pixel, the sea and the lakes per vertex as waterocean.fx
/// and lake.fx do. Colours in and out are gamma-encoded; 2070 fogs before
/// its GammaToLinear. Names start with `scene_`, as the kit's lighting
/// rules want.
pub const ATMOSPHERE_WGSL: &str = r#"
// `Scatter::words`. An atmosphere of zeros scatters nothing.
struct scene_atmosphere {
    // cHorizonColor, gamma; w: cSunFallOff
    horizon: vec4<f32>,
    // cZenithColor, gamma; w: the cosine of the sun's outer angle
    zenith: vec4<f32>,
    // cHorizonFallOff per channel; w: 1 / (cos inner - cos outer)
    horizon_falloff: vec4<f32>,
    // cDistanceFallOff per channel; w: where scattering starts
    distance_falloff: vec4<f32>,
    // x: 1 / how far (less 5 m) it runs to the far plane; y: the far
    // plane; z: how far the eye may back off the camera's anchor
    reach: vec4<f32>,
}

struct scene_scattering {
    extinction: vec3<f32>,
    inscatter: vec3<f32>,
}

// ScatteringVS: extinction and inscattered light at `world` seen from
// `eye`, the sun toward unit `sun`.
fn scene_scatter(a: scene_atmosphere, eye: vec3<f32>, world: vec3<f32>, sun: vec3<f32>) -> scene_scattering {
    if a.reach.x <= 0.0 { return scene_scattering(vec3<f32>(1.0), vec3<f32>(0.0)); }
    let d = world - eye;
    let distance = max(length(d), 1e-6);
    let view = d / distance;
    let offset = eye.y / a.reach.y;
    let reciprocal = 1.0 / (offset + 1.0);
    var sun_angle = dot(view, sun);
    let glow = pow(max(clamp(clamp(sun_angle, 0.0, 1.0) - a.zenith.w, 0.0, 1.0) * a.horizon_falloff.w, 0.0), a.horizon.w);
    sun_angle = clamp(sun_angle * sun_angle * 0.3 + 0.7, 0.0, 1.0);
    let zenith = 1.0 - clamp((view.y + offset) * reciprocal, 0.0, 1.0);
    let depth = clamp((distance - a.distance_falloff.w) * a.reach.x, 0.0, 1.0);
    let toward = depth * pow(vec3<f32>(max(zenith, 0.0)), a.horizon_falloff.rgb);
    let scattering = sun_angle * mix(a.zenith.rgb, a.horizon.rgb, toward) + vec3<f32>(0.5, 0.4, 0.1) * 0.7 * glow;
    let fade = pow(vec3<f32>(depth), a.distance_falloff.rgb);
    return scene_scattering(1.0 - fade, fade * scattering);
}

// ScatteringPS.
fn scene_scattered(color: vec3<f32>, s: scene_scattering) -> vec3<f32> {
    return color * s.extinction + s.inscatter;
}

// The scattering toward `world` as the frame's camera sees it: from its eye
// held within the game camera's reach of the anchor (scene.rs `held_eye`),
// the sun the frame's. The Top view, orthographic, sees none.
fn scene_view_scatter(a: scene_atmosphere, world: vec3<f32>) -> scene_scattering {
    if globals.eye.w < 0.5 { return scene_scattering(vec3<f32>(1.0), vec3<f32>(0.0)); }
    var eye = globals.eye.xyz;
    let back = eye - globals.anchor.xyz;
    if length(back) > a.reach.z { eye = globals.anchor.xyz + back * (a.reach.z / length(back)); }
    return scene_scatter(a, eye, world, lights.sun.xyz);
}

fn scene_gamma(c: vec3<f32>) -> vec3<f32> {
    return pow(max(c, vec3<f32>(1e-4)), vec3<f32>(1.0 / 2.2));
}

fn scene_linear(c: vec3<f32>) -> vec3<f32> {
    return pow(max(c, vec3<f32>(1e-4)), vec3<f32>(2.2));
}
"#;

fn lerp(a: f32, b: f32, t: f32) -> f32 {
    a + (b - a) * t
}

/// The engine's gamma (functions.fxh:19-25, with `GAMMA_MAX`).
pub fn to_linear(c: f32) -> f32 {
    c.max(1e-4).powf(2.2)
}

pub fn to_gamma(c: f32) -> f32 {
    c.max(1e-4).powf(1. / 2.2)
}

/// `eye` held within `MAX_DISTANCE` of the camera's `anchor`, as the fog
/// measures from it.
pub fn held_eye(eye: [f32; 3], anchor: [f32; 3]) -> [f32; 3] {
    let d = [eye[0] - anchor[0], eye[1] - anchor[1], eye[2] - anchor[2]];
    let l = (d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt();
    if l <= MAX_DISTANCE {
        return eye;
    }
    std::array::from_fn(|a| anchor[a] + d[a] * (MAX_DISTANCE / l))
}

/// Where 2070's lighting finds a profile's scattering among the frame's
/// host words (`gpu::Glow::host`): `Scatter::words`' five vec4 from vec4 3
/// on. The model family of 117 and 1800 (anno-render `a7m_models`) holds
/// vec4 0-2, which 2070 islands never draw: the under-water view's two
/// vec4 (`underwater::words`) sit there.
pub const HOST_VEC4: usize = 3;

/// Writes `profile`'s scattering and its water seen from below into the
/// frame's host words, where `lighting()`'s fog and its under-water view
/// read them: a profile switch rewrites these and compiles nothing.
pub fn host_words(profile: &Profile, words: &mut [f32; HOST_WORDS]) {
    let at = HOST_VEC4 * 4;
    words[at..at + 20].copy_from_slice(profile.scatter().words().as_flattened());
    let at = crate::underwater::HOST_VEC4 * 4;
    words[at..at + 8]
        .copy_from_slice(crate::underwater::words(&profile.water.under).as_flattened());
}

/// The scene's `shade` and `fog`, as WGSL the kit composes into every lit
/// pipeline (`Lighting`). The sun's direction and the sun and ambient
/// colours come from the kit's light each frame, and the profile's
/// scattering from the frame's host words (`host_words`), so one set of
/// pipelines lights every profile.
pub fn lighting() -> Lighting {
    let words = (HOST_VEC4..HOST_VEC4 + 5)
        .map(|k| format!("lights.host[{k}]"))
        .collect::<Vec<_>>()
        .join(", ");
    lighting_with(&words)
}

/// `lighting()` with the profile's scattering as `atmosphere`, five WGSL
/// vec4 expressions.
fn lighting_with(atmosphere: &str) -> Lighting {
    let shade = format!(
        r#"// 2070's light (deferredshading.fx:96-115), from the raw terms: the
// ambient lerped toward the sun, not added to, by (N·L + spec)·shadow left
// unclamped, in linear light and back to gamma, then the gamma albedo times
// it, lerped toward a constant {glow_base} by glow.
// The kit splits a glowing surface into albedo·(1 − glow) for `s` and
// albedo·glow·strength for `emissive`; summed back they give the albedo
// and its glow, which 2070 lights as one.
// Material AO (`s.occlusion`) sits inside the blend, as 2070's G-buffer AO
// does, so glow skips it.
// TODO: screen-space AO stays the kit's, multiplied into the
// finished colour after Opaque, which darkens glow too. 2070 wants its AO
// as a `shade()` input inside this blend (its AMBIENT_OCCLUSION_MODE reads
// it from the G-buffer's alpha): the kit's Occlusion stage would have to
// run before lighting, or hand the factor to `shade()`.
fn shade(world: vec3<f32>, s: Surface, emissive: vec3<f32>, has_normals: bool) -> vec3<f32> {{
    return scene_linear(scene_light(world, s, emissive, has_normals).color);
}}

// What the lighting pass leaves in its colour: gamma-encoded, with the
// glow share and the shadow the under-water view reads after it
// (deferredshading.fx:455-475).
struct scene_lit {{
    color: vec3<f32>,
    glow: f32,
    shadow: f32,
}}

fn scene_light(world: vec3<f32>, s: Surface, emissive: vec3<f32>, has_normals: bool) -> scene_lit {{
    var own = vec3<f32>(0.0);
    if lights.glow.w > 0.0 {{ own = emissive / lights.glow.w; }}
    let albedo = s.albedo + own;
    let total = albedo.r + albedo.g + albedo.b;
    var glow = 0.0;
    if total > 1e-6 {{ glow = clamp((own.r + own.g + own.b) / total, 0.0, 1.0); }}
    var facing = dot(s.normal, lights.sun.xyz);
    if !has_normals {{ facing = abs(facing); }}
    let half_way = normalize(toward_viewer(world) + lights.sun.xyz);
    let spec = pow(clamp(dot(s.normal, half_way), 0.0, 1.0), 100.0) * 10.0 * s.gloss;
    let shadowed = shadow(world, s.geometry);
    let intensity = max(0.0, facing + spec) * shadowed;
    let light = scene_gamma(mix(lights.sky.rgb, lights.sun_color.rgb, intensity) * s.occlusion);
    return scene_lit(scene_gamma(albedo) * mix(light, vec3<f32>({glow_base}), glow), glow, shadowed);
}}
"#,
        glow_base = format!("{GLOW_BASE:?}"),
    );
    let fog = format!(
        r#"// 2070's atmospheric scattering applied as c·Fex + Fin in gamma space
// (deferredshading.fx:409-423), per pixel as the deferred pass does. The
// Top view, orthographic, has no eye and no haze.
fn scene_profile() -> scene_atmosphere {{
    return scene_atmosphere({atmosphere});
}}

fn fog(color: vec3<f32>, world: vec3<f32>) -> vec3<f32> {{
    if globals.eye.w < 0.5 {{ return color; }}
    return scene_linear(scene_scattered(scene_gamma(color), scene_view_scatter(scene_profile(), world)));
}}
{ATMOSPHERE_WGSL}"#,
    );
    Lighting {
        shade: Some(shade),
        fog: Some(fog),
        // The view from below the surface, which Deep Ocean islands open
        // in (`underwater::MODE`).
        modes: vec![crate::underwater::mode()],
        ..Default::default()
    }
}

/// The families a 2070 island draws with: the kit's own and 2070's model
/// families (buildings, leaves), under 2070's lighting. One set for every
/// profile, so every renderer shares its pipelines. In
/// `a2070_buildings::surfaces`' order, as `a2070_buildings::families`, so
/// the ids its meshes carry name them in both.
pub fn families() -> Arc<Families> {
    static FAMILIES: OnceLock<Arc<Families>> = OnceLock::new();
    FAMILIES
        .get_or_init(|| {
            Families::with_lighting(anno_render::a2070_buildings::surfaces(), lighting())
                .expect("2070's lighting registers")
        })
        .clone()
}

/// 2070's model look's families: the buildings' (anno-render), and on
/// islands the same under the scene's lighting (`families`).
pub struct Look;

impl anno_core::files::Surfaces for Look {
    fn families(&self) -> Arc<Families> {
        anno_render::a2070_buildings::families()
    }
    // One set for every profile: each profile's scattering reaches it
    // through the frame's host words (`ambient::Sky::host_words`).
    fn lit(&self) -> Option<Arc<Families>> {
        Some(families())
    }
    fn dress(
        &self,
        _look: &anno_core::files::ModelLook,
        material: &anno_formats::model::Material,
        data: &mut gpui_editor_kit::gpu::MeshData,
        texture: &mut dyn FnMut(&str) -> Option<gpui_editor_kit::texture::Image>,
    ) {
        anno_render::a2070_buildings::dress(material, data, texture);
    }
}

/// Every profile in `xml` (`ambientmap.xml`), three settings per name in
/// file order, and the zone defaults.
#[derive(Clone, Debug, PartialEq)]
pub struct AmbientMap {
    pub profiles: Vec<Profile>,
    /// `GlobalAreas`: each area's name and the profile name it lights by.
    pub areas: Vec<(String, String)>,
}

impl AmbientMap {
    pub fn read(install: &Install) -> Result<Self> {
        let bytes = install.read(AMBIENT_MAP)?;
        Self::parse(&String::from_utf8_lossy(&bytes)).context(AMBIENT_MAP)
    }

    pub fn parse(xml: &str) -> Result<Self> {
        let settings = xml::block(xml, "Settings").context("no <Settings>")?;
        let mut profiles = Vec::new();
        for (name, body) in children(settings) {
            for k in 0..3 {
                let tag = format!("Setting{k}");
                let setting =
                    xml::block(body, &tag).with_context(|| format!("{name}: no {tag}"))?;
                let profile = profile(format!("{name}:{k}"), setting)
                    .with_context(|| format!("{name} {tag}"))?;
                profiles.push(profile);
            }
        }
        let mut areas = Vec::new();
        if let Some(list) = xml::block(xml, "GlobalAreas") {
            for (_, area) in children(list) {
                let name = xml::tag(area, "Name").context("area without <Name>")?;
                let setting = xml::tag(area, "Setting").unwrap_or_default();
                areas.push((name.to_owned(), setting.to_owned()));
            }
        }
        Ok(Self { profiles, areas })
    }

    /// The profile named `name` (`<name>:<setting>`, or a bare name for its
    /// first setting).
    pub fn get(&self, name: &str) -> Option<&Profile> {
        let full;
        let name = if name.contains(':') {
            name
        } else {
            full = format!("{name}:0");
            &full
        };
        self.profiles.iter().find(|p| p.name == name)
    }

    /// The profile name the zone default gives an island with none of its
    /// own: the `NorthIsland` area's, or `Underwater_Island_North`'s
    /// (0x140B7AB30 asks for these two by name).
    pub fn island_default(&self, underwater: bool) -> Option<&str> {
        let area = if underwater {
            "Underwater_Island_North"
        } else {
            "NorthIsland"
        };
        (self.areas.iter())
            .find(|(name, _)| name == area)
            .map(|(_, setting)| setting.as_str())
            .filter(|s| !s.is_empty())
    }
}

/// The profile lighting island `path` (virtual), and what decided it: the
/// zone each shipped world placing it gives it (its entry's own
/// `LightProfile`, else the zone default), the most common one across
/// worlds, the earliest world winning a tie; with no world placing it, the
/// zone default. `underwater` as the island lies.
pub fn island_profile(
    install: &Install,
    map: &AmbientMap,
    path: &str,
    underwater: bool,
) -> Option<String> {
    let default = map.island_default(underwater);
    let path = path.replace('\\', "/");
    // (name, count), in the order first seen: a handful at most.
    let mut seen: Vec<(String, u32)> = Vec::new();
    for id in (0..install.file_count()).map(|i| FileId(i as u32)) {
        if !install.file_path(id).ends_with(".www") {
            continue;
        }
        let Ok(bytes) = install.read_file(id) else {
            continue;
        };
        let Ok(doc) = Doc::parse(&bytes) else {
            continue;
        };
        for (file, own) in placed_profiles(&doc) {
            if !file.replace('\\', "/").eq_ignore_ascii_case(&path) {
                continue;
            }
            let Some(name) = Some(own.as_str()).filter(|s| !s.is_empty()).or(default) else {
                continue;
            };
            match seen.iter_mut().find(|(n, _)| n == name) {
                Some((_, count)) => *count += 1,
                None => seen.push((name.to_owned(), 1)),
            }
        }
    }
    // `max_by_key` keeps the last of equals; the first seen wins instead.
    let best = (seen.iter().rev()).max_by_key(|(_, count)| *count);
    best.map(|(name, _)| name.clone())
        .or(default.map(str::to_owned))
}

/// Each island a world places: its `Filename` and its `LightProfile`
/// (empty where it has none).
fn placed_profiles(doc: &Doc) -> Vec<(String, String)> {
    let text = |n: Option<NodeId>| match n.map(|n| doc.value(n)) {
        Some(Value::Text(t)) => String::from_utf8_lossy(t).into_owned(),
        _ => String::new(),
    };
    let Some(islands) = (doc.root("WorldConfig")).and_then(|w| doc.child(w, "Islands")) else {
        return Vec::new();
    };
    (doc.children(islands))
        .filter(|&v| doc.name(v) == b"v")
        .filter_map(|v| doc.child(v, "IslandConfig"))
        .map(|c| {
            (
                text(doc.child(c, "Filename")),
                text(doc.child(c, "LightProfile")),
            )
        })
        .collect()
}

/// The direct children of an XML body, each `(name, body)`, in order.
fn children(xml: &str) -> Vec<(&str, &str)> {
    let mut out = Vec::new();
    let mut rest = xml;
    while let Some(open) = rest.find('<') {
        let after = &rest[open + 1..];
        let Some(close) = after.find('>') else { break };
        let name = &after[..close];
        if name.starts_with('/') || name.is_empty() {
            break;
        }
        let Some(body) = xml::block(&rest[open..], name) else {
            break;
        };
        out.push((name, body));
        // Past the body and its closing tag.
        let end = open + 1 + close + 1 + body.len();
        rest = &rest[end..];
        match rest.find('>') {
            Some(gt) => rest = &rest[gt + 1..],
            None => break,
        }
    }
    out
}

fn profile(name: String, s: &str) -> Result<Profile> {
    let water = xml::block(s, "Water").context("no <Water>")?;
    let atmosphere = xml::block(s, "Atmosphere").context("no <Atmosphere>")?;
    // The colours and powers outside the two blocks: read the setting with
    // them cut out, so `SunColor` and friends find the atmosphere's alone.
    let outer = s.replace(water, "").replace(atmosphere, "");
    let o = outer.as_str();
    let w = water;
    let a = atmosphere;
    let water_light = |side: &str| -> Result<WaterLight> {
        Ok(WaterLight {
            absorption: vector(w, &format!("{side}WaterLightAbsorptionExponents"))?,
            light_depth: number(w, &format!("{side}WaterLightDepth"))?,
            view_depth: number(w, &format!("{side}WaterViewDepth"))?,
            view_depth_exponent: number(w, &format!("{side}WaterViewDepthExponent"))?,
            scattering: vector(w, &format!("{side}WaterScattering"))?,
        })
    };
    Ok(Profile {
        name,
        light: Light {
            diffuse: color(o, "DiffuseColor")?,
            diffuse_power: number(o, "DiffuseColorPower")?,
            ambient: color(o, "AmbientColor")?,
            ambient_power: number(o, "AmbientColorPower")?,
            mood: vector(o, "MoodColor")?,
            mood_power: number(o, "MoodColorPower")?,
            inv_cloud_shadow: number(o, "InvCloudShadowFac")?,
            sun_color_power: number(o, "SunColorPower")?,
            horizon_color_power: number(o, "HorizonColorPower")?,
            zenith_color_power: number(o, "ZenithColorPower")?,
            cloud_color_power: number(o, "CloudColorPower")?,
        },
        water: Water {
            wave_frequency: vector(w, "WaveFrequency")?,
            wave_amplitude: vector(w, "WaveAmplitude")?,
            wave_velocity: [vector(w, "WaveVelocity0")?, vector(w, "WaveVelocity1")?],
            choppiness: number(w, "Choppiness")?,
            above: water_light("Above")?,
            under: water_light("Under")?,
            under_dof_focus: number(w, "UnderWaterDOFFocusDistance")?,
        },
        atmosphere: Atmosphere {
            scatter_start: number(a, "ScatterStart")?,
            sun_inner_angle: number(a, "SunInnerAngle")?,
            sun_outer_angle: number(a, "SunOuterAngle")?,
            sun_falloff: number(a, "SunFallOff")?,
            horizon_falloff: number(a, "HorizonFallOff")?,
            distance_falloff: number(a, "DistanceFallOff")?,
            cloud_density: number(a, "CloudDensity")?,
            sun_color: color(a, "SunColor")?,
            horizon_color: color(a, "HorizonColor")?,
            zenith_color: color(a, "ZenithColor")?,
            cloud_color: color(a, "CloudColor")?,
        },
        wind: vector(o, "Wind")?,
        current: vector(o, "Current")?,
        blur_strength: number(o, "BlurStrength")?,
        light_direction: vector(o, "LightDirection")?,
        cam_y_rot_offset: number(o, "CamYRotOffset")?,
    })
}

fn number(xml: &str, name: &str) -> Result<f32> {
    let text = xml::tag(xml, name).with_context(|| format!("no <{name}>"))?;
    text.parse()
        .with_context(|| format!("<{name}>: {text:?} is not a number"))
}

/// `<name>` holding `<x><y>..` or `<r><g>..` children, as many as `N`.
fn vector<const N: usize>(xml: &str, name: &str) -> Result<[f32; N]> {
    let body = xml::block(xml, name).with_context(|| format!("no <{name}>"))?;
    let parts = children(body);
    if parts.len() < N {
        bail!("<{name}>: {} parts, wanted {N}", parts.len());
    }
    let mut out = [0.; N];
    for (slot, (part, text)) in out.iter_mut().zip(parts) {
        *slot = (text.trim().parse())
            .with_context(|| format!("<{name}><{part}>: {text:?} is not a number"))?;
    }
    Ok(out)
}

fn color(xml: &str, name: &str) -> Result<[f32; 3]> {
    let [r, g, b, _] = vector::<4>(xml, name)?;
    Ok([r, g, b])
}

/// One profile as the editor lights an island: the sun from its fixed
/// direction at every hour, its sun and ambient colours as the kit's light,
/// the sky's light painted from its scattering.
#[derive(Debug)]
pub struct Sky {
    pub profile: Profile,
}

impl Sky {
    /// The sky the scattering paints around an eye at sea level, out to the
    /// far plane, linear: a backdrop and fog colour until 2070's own sky
    /// pass draws.
    fn map(&self) -> SkyMap {
        let (width, height) = (64, 32);
        let scatter = self.profile.scatter();
        let sun = self.profile.toward_sun();
        let texels = (0..height)
            .flat_map(|y| (0..width).map(move |x| (x, y)))
            .map(|(x, y)| {
                let view = SkyMap::direction(width, height, x, y);
                let (_, inscatter) = scatter.along(0., view, FAR, sun);
                inscatter.map(to_linear)
            })
            .collect();
        SkyMap {
            width,
            height,
            texels,
        }
    }
}

impl anno_core::ambient::Sky for Sky {
    fn sun_moon(&self, _hours: f32) -> SunMoon {
        let sun = self.profile.toward_sun();
        SunMoon { sun, moon: sun }
    }

    fn bake(&self, _hours: f32) -> Baked {
        Baked {
            map: Arc::new(self.map()),
            toward: self.profile.toward_sun(),
            light: self.profile.sun().map(to_linear),
            disc: [0.; 3],
            ambient: None,
        }
    }

    // 2070 writes its lit colour linear and the display shows it gamma
    // encoded: the curve's gamma undoes the linearising, so what the game
    // shows under 1 shows as it is. Glow's ×8 runs past it.
    // TODO: 2070's HDR tonemapping (postfx_hdr_tonemapping) in
    // place of the shoulder, once a capture shows what it does.
    fn curve(&self) -> Option<Curve> {
        Some(Curve::Shoulder {
            gamma: 2.2,
            max_linear: 1.,
            white: 8.,
        })
    }

    // No day and night: the profile is the time.
    fn day_ratio(&self, _hours: f32) -> f32 {
        1.
    }

    fn host_words(&self, words: &mut [f32; HOST_WORDS]) {
        host_words(&self.profile, words);
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

/// The ambients among `names` that `install`'s map has, lit by each.
pub fn ambients(install: &Install, names: &[&str]) -> Vec<(String, Ambient)> {
    let map = match AmbientMap::read(install) {
        Ok(map) => map,
        Err(e) => {
            eprintln!("2070 light profiles unavailable: {e:#}");
            return Vec::new();
        }
    };
    (names.iter())
        .filter_map(|&name| Some((name.to_owned(), ambient(map.get(name)?.clone()))))
        .collect()
}

/// `profile` as the editor's ambient. The kit's light takes the profile's
/// sun and ambient colours linear, as 2070's lighting reads them
/// (`GammaToLinear(cLgtDiffuse)`, deferredshading.fx:107). Its sea is the
/// profile's own Water block, so every profile draws the water pass.
pub fn ambient(profile: Profile) -> Ambient {
    let sun = profile.sun().map(to_linear);
    let sky_light = profile.ambient().map(to_linear);
    let scatter = profile.scatter();
    Ambient {
        sun,
        sky_light,
        zenith: scatter.zenith.map(to_linear),
        horizon: scatter.horizon.map(to_linear),
        // 2070's light has no ground bounce: its ambient is one colour.
        ground: sky_light,
        exposure: 1.,
        sun_occlusion: 0.,
        // Glow's emissive arrives unscaled (`lights.glow.w` 1), and 2070
        // has no night masks.
        glow: 1.,
        night_glow: 0.,
        sea: Some(Arc::new(crate::water::Sea::new(&profile))),
        sky: Some(Arc::new(Sky { profile })),
        ..Ambient::default()
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use gpui_editor_kit::gpu::Family;

    fn install() -> Option<Install> {
        let Some(dir) = anno_formats::vfs::install_dir(crate::PROFILE.env_var) else {
            eprintln!("skip: ANNO2070_DIR unset");
            return None;
        };
        Some(Install::open(&dir, crate::ARCHIVE_FAMILIES).unwrap())
    }

    // An island's families, the look's under the scene's lighting, hold
    // the building family at the id its meshes are dressed with; every
    // profile lights through the one set.
    #[test]
    fn island_families_draw_the_building_family() {
        use anno_render::a2070_buildings::{NAME, families as buildings};
        let id = families().id(NAME);
        assert!(id.is_some() && id == buildings().id(NAME));
        let module: &dyn anno_core::game::Module = &crate::MODULE;
        let lit = module.model_look().surfaces.unwrap().0.lit();
        assert!(Arc::ptr_eq(&lit.unwrap(), &families()));
    }

    // Every number a profile holds, alphas of its plain colours left out as
    // the parse leaves them.
    fn sum(p: &Profile) -> f64 {
        let l = &p.light;
        let w = &p.water;
        let a = &p.atmosphere;
        let mut v: Vec<f32> = Vec::new();
        v.extend(l.diffuse);
        v.push(l.diffuse_power);
        v.extend(l.ambient);
        v.push(l.ambient_power);
        v.extend(l.mood);
        v.extend([
            l.mood_power,
            l.inv_cloud_shadow,
            l.sun_color_power,
            l.horizon_color_power,
            l.zenith_color_power,
            l.cloud_color_power,
        ]);
        v.extend(w.wave_frequency);
        v.extend(w.wave_amplitude);
        v.extend(w.wave_velocity.concat());
        v.push(w.choppiness);
        for s in [&w.above, &w.under] {
            v.extend(s.absorption);
            v.extend([s.light_depth, s.view_depth, s.view_depth_exponent]);
            v.extend(s.scattering);
        }
        v.push(w.under_dof_focus);
        v.extend([
            a.scatter_start,
            a.sun_inner_angle,
            a.sun_outer_angle,
            a.sun_falloff,
            a.horizon_falloff,
            a.distance_falloff,
            a.cloud_density,
        ]);
        for c in [a.sun_color, a.horizon_color, a.zenith_color, a.cloud_color] {
            v.extend(c);
        }
        v.extend(p.wind);
        v.extend(p.current);
        v.push(p.blur_strength);
        v.extend(p.light_direction);
        v.push(p.cam_y_rot_offset);
        v.iter().map(|&x| f64::from(x)).sum()
    }

    // All 99 profiles (33 names, three settings each) parse whole. The
    // sum of every number they hold was taken independently from the file
    // (a Python walk over each setting's leaves), and a few values are
    // checked where they sit.
    #[test]
    fn all_99_profiles_parse_with_their_recorded_values() {
        let Some(install) = install() else { return };
        let map = AmbientMap::read(&install).unwrap();
        assert_eq!(map.profiles.len(), 99);
        let names: std::collections::BTreeSet<&str> = (map.profiles.iter())
            .map(|p| p.name.split(':').next().unwrap())
            .collect();
        assert_eq!(names.len(), 33);
        let total: f64 = map.profiles.iter().map(sum).sum();
        assert!((total - 22049.975456).abs() < 1e-2, "sum {total}");

        let north = map.get("North").unwrap();
        assert_eq!(north.name, "North:0");
        assert_eq!(north.light.diffuse, [0.7176471, 0.5882353, 0.4862745]);
        assert_eq!(north.light.diffuse_power, 2.1);
        assert_eq!(north.light.ambient_power, 1.95);
        assert_eq!(north.light.mood, [0.3529412, 0.3490196, 0.3294118, 0.2]);
        assert_eq!(north.water.above.absorption, [3.88, 0.75, 0.1]);
        assert_eq!(north.water.under.view_depth, 58.6);
        assert_eq!(north.atmosphere.scatter_start, 0.095);
        assert_eq!(north.atmosphere.sun_outer_angle, 0.8435175);
        assert_eq!(
            north.atmosphere.sun_color,
            [0.1882353, 0.7058824, 0.8117647]
        );
        assert_eq!(north.light_direction, [-0.5896695, -0.7416229, -0.3198208]);
        let north1 = map.get("North:1").unwrap();
        assert_eq!(north1.light.diffuse_power, 1.7);
        assert_eq!(north1.atmosphere.horizon_falloff, 19.584);
        let sundown = map.get("Sundown:2").unwrap();
        assert_eq!(sundown.light_direction, [0.5306998, -0.4614388, 0.7109374]);
        assert_eq!(
            sundown.atmosphere.zenith_color,
            [0.3411765, 0.3058824, 0.2705882]
        );
        assert_eq!(
            sundown.atmosphere.cloud_color,
            [0.6156863, 0.3803922, 0.3176471]
        );

        assert_eq!(map.island_default(false), Some("North"));
        assert_eq!(map.island_default(true), Some("Underwater_Island"));
    }

    // Shipped islands take the profile the worlds placing them give their
    // zone: their entry's own (both worlds placing the sinking island say
    // North_RivalIsland), the zone default where an entry names none
    // (n_d01: 14 North and 10 unnamed against 3 Chap3_Fallout), and the
    // default alone for an island no world places.
    #[test]
    fn a_shipped_island_resolves_its_profile() {
        let Some(install) = install() else { return };
        let map = AmbientMap::read(&install).unwrap();
        let resolve = |path: &str, underwater| island_profile(&install, &map, path, underwater);
        let campaign = "data/levels/islands/campaign/chapter 01";
        assert_eq!(
            resolve(&format!("{campaign}/sinking_island_m01.isd"), false).as_deref(),
            Some("North_RivalIsland")
        );
        assert_eq!(
            resolve(&format!("{campaign}/u_d14_01.isd"), true).as_deref(),
            Some("M14_Underwater_Island_Dark")
        );
        assert_eq!(
            resolve("data/levels/islands/normal/n_d01.isd", false).as_deref(),
            Some("North")
        );
        assert_eq!(
            resolve("data/levels/islands/nowhere.isd", false).as_deref(),
            Some("North")
        );
        assert_eq!(
            resolve("data/levels/islands/nowhere.isd", true).as_deref(),
            Some("Underwater_Island")
        );
        // Through the module, as the editor asks: the sinking island lies
        // above water, so its zone's light is the one named.
        let module: &dyn anno_core::game::Module = &crate::MODULE;
        let scene = module.scene().unwrap();
        let path = format!("{campaign}/sinking_island_m01.isd");
        let name = scene.island_ambient(&install, &path).unwrap();
        let lights = scene.ambients(&install, &[name.as_str()]);
        assert_eq!(lights.len(), 1);
        let rival = map.get("North_RivalIsland").unwrap();
        assert_eq!(lights[0].1.sun, rival.sun().map(to_linear));
        let sky = lights[0].1.sky.as_ref().unwrap();
        let toward = sky.sun_moon(12.).sun;
        assert!(toward[1] > 0.);
        assert_eq!(toward, rival.toward_sun());
    }

    /// North's first setting, as `ambientmap.xml` holds it, three times
    /// over: the formula tests need no install.
    pub(crate) fn north() -> Profile {
        let setting = include_str!("../tests/north_setting.xml");
        let xml = format!(
            "<AmbientMap><Settings><North><Setting0>{setting}</Setting0>\
             <Setting1>{setting}</Setting1><Setting2>{setting}</Setting2>\
             </North></Settings></AmbientMap>"
        );
        AmbientMap::parse(&xml).unwrap().profiles.remove(0)
    }

    // Draws whatever the instance word says: glow in byte 0, the spec mask
    // in byte 1, material AO in byte 2 and the albedo's scale in byte 3,
    // facing straight up, split as the kit splits a glowing surface.
    pub(crate) fn probe() -> Family {
        let wgsl = r#"
fn probe_coverage(material: GpuDraw, c: Cover) -> f32 {
    return 1.0;
}

fn probe_surface(material: GpuDraw, p: LitPixel, t: LitTexels) -> Lit {
    let w = p.word;
    let glow = f32(w & 255u) / 255.0;
    let albedo = vec3<f32>(0.5, 0.35, 0.2) * (f32(w >> 24u) / 255.0);
    var s = plain(albedo * (1.0 - glow), vec3<f32>(0.0, 1.0, 0.0));
    s.gloss = f32((w >> 8u) & 255u) / 255.0;
    s.occlusion = f32((w >> 16u) & 255u) / 255.0;
    return Lit(s, albedo * glow * lights.glow.w);
}
"#;
        Family {
            name: "probe".into(),
            wgsl: wgsl.into(),
            light: false,
            vertex: false,
            coverage_slots: 0,
            bake: false,
        }
    }

    /// The scene's glow for `profile`: full strength, its scattering in the
    /// host words.
    pub(crate) fn profile_glow(profile: &Profile) -> gpui_editor_kit::gpu::Glow {
        let mut glow = gpui_editor_kit::gpu::Glow {
            strength: 1.,
            ..Default::default()
        };
        host_words(profile, &mut glow.host);
        glow
    }

    /// North with another atmosphere and light: every scattering constant
    /// moved.
    fn dusk() -> Profile {
        let mut p = north();
        p.name = "Dusk:0".into();
        let a = &mut p.atmosphere;
        a.scatter_start = 0.02;
        a.horizon_falloff *= 0.5;
        a.distance_falloff *= 1.7;
        a.sun_falloff += 2.;
        a.sun_inner_angle *= 0.8;
        a.horizon_color = [0.9, 0.45, 0.2];
        a.zenith_color = [0.25, 0.2, 0.35];
        p.light.diffuse = [0.9, 0.5, 0.3];
        p
    }

    // Probe quads from near to far out, glowing and not, placed once, then
    // a frame lit and fogged under each profile's light and glow words in
    // turn.
    fn probe_frames(
        renderer: &gpui_editor_kit::gpu::Renderer,
        profiles: &[&Profile],
    ) -> Vec<Vec<u8>> {
        use gpui_editor_kit::camera::{Camera, Extent};
        use gpui_editor_kit::gpu::{
            Light, Material, Mesh, MeshData, Offscreen, Piece, Scene, Tone,
        };
        use gpui_editor_kit::gpui::{point, px};
        use gpui_editor_kit::math;
        let camera = Camera {
            fov: 0.9,
            tilt: 1.1,
            yaw: 0.4,
            ..Camera::default()
        };
        let world = Extent {
            center: [0.; 3],
            size: 40.,
        };
        let mut target = Offscreen::new(240, 160).unwrap();
        let quad = MeshData {
            positions: vec![
                [-0.5, 0., -0.5],
                [0.5, 0., -0.5],
                [0.5, 0., 0.5],
                [-0.5, 0., 0.5],
            ],
            normals: vec![[0., 1., 0.]; 4],
            indices: vec![0, 1, 2, 0, 2, 3, 0, 2, 1, 0, 3, 2],
            material: Material::Lit,
            family: renderer.families().id("probe").unwrap(),
            ..MeshData::default()
        };
        // A quad under each of a grid of pixels, sized to its distance.
        let bounds = target.bounds();
        let (mut rows, mut words) = (Vec::new(), Vec::new());
        for i in 0..10u32 {
            for j in 0..8u32 {
                let (x, y) = (j as f32 * 30. + 15., 158. - (i * i) as f32 * 1.6);
                let ray = camera.ray(point(px(x), px(y)), bounds, world);
                if ray.direction[1] >= 0. {
                    continue;
                }
                let t = -ray.origin[1] / ray.direction[1];
                let at: [f32; 3] =
                    std::array::from_fn(|a| (ray.origin[a] + ray.direction[a] * t) as f32);
                // Each a little above the last: overlapping neighbours
                // would otherwise fight over depth, which a draw order
                // decides.
                let lift = rows.len() as f32 * 0.02;
                let mut m = math::translate([at[0], lift, at[2]]);
                m[0] = (0.2 * t as f32).max(2.);
                m[10] = m[0];
                rows.push(m);
                // Glow, spec mask, AO, albedo scale.
                words.push(if (i + j) % 3 == 0 {
                    0x0480_ff40
                } else {
                    0xc0ff_6000
                });
            }
        }
        assert!(rows.len() > 60);
        renderer.set_pieces(vec![Piece {
            levels: vec![(Arc::new(Mesh::new(quad).unwrap()), 0.)],
            impostor: None,
            pose: None,
        }]);
        renderer.set_placements(&vec![0; rows.len()], &rows);
        renderer.set_instance_words(0, &words);
        (profiles.iter())
            .map(|profile| {
                renderer.set_scene(&Scene {
                    light: Light {
                        sun: profile.toward_sun(),
                        sun_color: profile.sun().map(to_linear),
                        sky: profile.ambient().map(to_linear),
                        ground: profile.ambient().map(to_linear),
                        shadows: None,
                        fog: None,
                    },
                    tone: Some(Tone {
                        exposure: 1.,
                        sun_occlusion: 0.,
                        ambient: 1.,
                        curve: Curve::Shoulder {
                            gamma: 2.2,
                            max_linear: 1.,
                            white: GLOW_BASE,
                        },
                        auto: None,
                    }),
                    glow: profile_glow(profile),
                    ..Default::default()
                });
                target.render(renderer, camera, world).unwrap()
            })
            .collect()
    }

    // A profile switch rewrites the frame's host words and builds nothing:
    // one renderer, made once with the one family set every profile shares,
    // draws two profiles and back, and each frame is byte for byte the
    // frame of lighting with that profile's constants written into the
    // WGSL, as 2070's lighting was built before the host words.
    #[test]
    fn switching_profiles_rewrites_words_and_draws_as_the_baked_lighting() {
        use gpui_editor_kit::gpu::Renderer;
        let words = |p: &Profile| {
            (p.scatter().words().iter())
                .map(|w| format!("vec4<f32>({:?}, {:?}, {:?}, {:?})", w[0], w[1], w[2], w[3]))
                .collect::<Vec<_>>()
                .join(", ")
        };
        let baked = |p: &Profile| {
            let families =
                Families::with_lighting(vec![probe()], lighting_with(&words(p))).unwrap();
            probe_frames(&Renderer::with_families(families), &[p]).remove(0)
        };
        let shared = Families::with_lighting(vec![probe()], lighting()).unwrap();
        let renderer = Renderer::with_families(shared.clone());
        let (north, dusk) = (north(), dusk());
        let frames = probe_frames(&renderer, &[&north, &dusk, &north]);
        let [a, b, back] = <[Vec<u8>; 3]>::try_from(frames).unwrap();
        assert!(Arc::ptr_eq(&renderer.families(), &shared));
        let moved = a.chunks(4).zip(b.chunks(4)).filter(|(p, q)| p != q).count();
        assert!(
            moved > 240 * 160 / 4,
            "the profiles drew {moved} pixels apart"
        );
        assert!(a == back, "switching back changed the frame");
        // (pixels apart, the largest step) from the baked frame.
        let apart = |x: &[u8], y: &[u8]| {
            let n = x.chunks(4).zip(y.chunks(4)).filter(|(p, q)| p != q).count();
            let worst = x.iter().zip(y).map(|(p, q)| p.abs_diff(*q)).max();
            (n, worst.unwrap_or(0))
        };
        let (north_baked, dusk_baked) = (baked(&north), baked(&dusk));

        eprintln!(
            "against the baked lighting: North {:?}, Dusk {:?}",
            apart(&a, &north_baked),
            apart(&b, &dusk_baked)
        );
        assert!(a == north_baked, "North drew apart from its baked lighting");
        assert!(b == dusk_baked, "Dusk drew apart from its baked lighting");
    }

    /// What 2070 shows for the probe's surface at `world`, seen from `eye`
    /// (the camera's anchor at `anchor`), the sun toward `sun`: its
    /// lighting (deferredshading.fx:96-115), then the scattering, gamma
    /// encoded, as the display shows it.
    pub(crate) fn reference(
        profile: &Profile,
        sun: [f32; 3],
        eye: [f32; 3],
        anchor: [f32; 3],
        world: [f32; 3],
        [glow, mask, ao, scale]: [f32; 4],
        fogged: bool,
    ) -> [f32; 3] {
        let norm = |v: [f32; 3]| {
            let l = (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt();
            v.map(|c| c / l)
        };
        let albedo = [0.5, 0.35, 0.2].map(|c: f32| c * scale);
        let view = norm([eye[0] - world[0], eye[1] - world[1], eye[2] - world[2]]);
        let half = norm(std::array::from_fn(|a| view[a] + sun[a]));
        let spec = half[1].clamp(0., 1.).powf(100.) * 10. * mask;
        let intensity = (sun[1] + spec).max(0.);
        let (sun_light, ambient) = (
            profile.sun().map(to_linear),
            profile.ambient().map(to_linear),
        );
        let (fex, fin) = match fogged {
            true => profile.scatter().at(held_eye(eye, anchor), world, sun),
            false => ([1.; 3], [0.; 3]),
        };
        std::array::from_fn(|c| {
            let light = to_gamma(lerp(ambient[c], sun_light[c], intensity) * ao);
            let lit = to_gamma(albedo[c]) * lerp(light, GLOW_BASE, glow);
            lit * fex[c] + fin[c]
        })
    }

    // The scene's shade and fog on the GPU match 2070's formulas worked on
    // the CPU, pixel by pixel: 24 flat surfaces from 15 m to 290 m out,
    // past where the scattering starts (35 m), with glow, spec mask, AO and
    // albedo varied, under three suns: the profile's own, a low one, and
    // one mirrored into the view for a strong highlight. Within 3 levels of
    // 255: the target's own rounding and the GPU's pow.
    #[test]
    fn shade_and_fog_match_2070s_formulas_on_the_gpu() {
        use gpui_editor_kit::camera::{Camera, Extent};
        use gpui_editor_kit::gpu::{
            Light, Material, Mesh, MeshData, Offscreen, Piece, Renderer, Scene, Tone,
        };
        use gpui_editor_kit::gpui::{point, px};
        use gpui_editor_kit::math;

        let profile = north();
        let families = Families::with_lighting(vec![probe()], lighting()).unwrap();
        let (w, h) = (480u32, 320u32);
        let mut target = Offscreen::new(w, h).unwrap();
        let camera = Camera {
            fov: 0.9,
            tilt: 1.1,
            yaw: 0.4,
            ..Camera::default()
        };
        let world = Extent {
            center: [0.; 3],
            size: 40.,
        };
        let bounds = target.bounds();
        let eye = camera.eye(bounds, world).unwrap();
        let anchor = camera.anchor(world);
        // Glow, mask, AO, albedo scale: dark albedo where glow would
        // otherwise run past white.
        let looks: [[f32; 4]; 8] = [
            [0., 0., 1., 1.],
            [0., 1., 1., 0.8],
            [0.5, 0.5, 0.8, 0.02],
            [1., 0., 1., 0.01],
            [0.25, 1., 0.6, 0.1],
            [0., 0.5, 0.4, 1.],
            [0.75, 0., 1., 0.005],
            [0., 1., 0.9, 0.5],
        ];
        let byte = |x: f32| (x * 255.).round() as u32;
        let mut pixels_at = Vec::new();
        let mut rows = Vec::new();
        let mut words = Vec::new();
        let mut cases = Vec::new();
        for (r, y) in [300u32, 220, 150, 90, 45, 22].into_iter().enumerate() {
            for (c, x) in [60u32, 180, 300, 420].into_iter().enumerate() {
                let ray = camera.ray(point(px(x as f32 + 0.5), px(y as f32 + 0.5)), bounds, world);
                assert!(ray.direction[1] < 0., "pixel {x},{y} sees no ground");
                let t = -ray.origin[1] / ray.direction[1];
                let at: [f32; 3] =
                    std::array::from_fn(|a| (ray.origin[a] + ray.direction[a] * t) as f32);
                let distance = t as f32;
                let size = (0.2 * distance).max(2.);
                let mut m = math::translate(at);
                m[0] = size;
                m[10] = size;
                let look = looks[(r * 4 + c) % looks.len()];
                // Quantised as the word carries it.
                let look = look.map(|v| byte(v) as f32 / 255.);
                rows.push(m);
                words.push(
                    byte(look[0]) | byte(look[1]) << 8 | byte(look[2]) << 16 | byte(look[3]) << 24,
                );
                pixels_at.push((x, y));
                cases.push((at, look, distance));
            }
        }
        let far = cases.iter().map(|c| c.2).fold(0., f32::max);
        assert!(far > 250., "farthest {far} m");
        let quad = MeshData {
            positions: vec![
                [-0.5, 0., -0.5],
                [0.5, 0., -0.5],
                [0.5, 0., 0.5],
                [-0.5, 0., 0.5],
            ],
            normals: vec![[0., 1., 0.]; 4],
            indices: vec![0, 1, 2, 0, 2, 3, 0, 2, 1, 0, 3, 2],
            material: Material::Lit,
            family: families.id("probe").unwrap(),
            ..MeshData::default()
        };
        let toward = profile.toward_sun();
        let low = {
            let e = 0.17f32;
            [0.8 * e.cos(), e.sin(), -0.6 * e.cos()]
        };
        // The sun mirrored through the vertical from the view: N·H near 1
        // over the middle of the frame.
        let mirrored = {
            let v = camera.toward();
            [-v[0], v[1], -v[2]]
        };
        let mut worst = 0f32;
        // How many cases each term moves by more than 10 levels: fog, glow,
        // spec and AO each have to, or the match proves nothing of it.
        let mut moved = [0u32; 4];
        for sun in [toward, low, mirrored] {
            let renderer = Renderer::with_families(families.clone());
            renderer.set_scene(&Scene {
                light: Light {
                    sun,
                    sun_color: profile.sun().map(to_linear),
                    sky: profile.ambient().map(to_linear),
                    ground: profile.ambient().map(to_linear),
                    shadows: None,
                    fog: None,
                },
                tone: Some(Tone {
                    exposure: 1.,
                    sun_occlusion: 0.,
                    ambient: 1.,
                    curve: Curve::Shoulder {
                        gamma: 2.2,
                        max_linear: 1.,
                        white: GLOW_BASE,
                    },
                    auto: None,
                }),
                glow: profile_glow(&profile),
                ..Default::default()
            });
            renderer.set_pieces(vec![Piece {
                levels: vec![(Arc::new(Mesh::new(quad.clone()).unwrap()), 0.)],
                impostor: None,
                pose: None,
            }]);
            renderer.set_placements(&vec![0; rows.len()], &rows);
            renderer.set_instance_words(0, &words);
            let pixels = target.render(&renderer, camera, world).unwrap();
            for (&(x, y), &(at, look, distance)) in pixels_at.iter().zip(&cases) {
                let i = ((y * w + x) * 4) as usize;
                let got = [pixels[i], pixels[i + 1], pixels[i + 2]];
                let want = reference(&profile, sun, eye, anchor, at, look, true)
                    .map(|c| c.clamp(0., 1.) * 255.);
                let off = (0..3)
                    .map(|c| (f32::from(got[c]) - want[c]).abs())
                    .fold(0., f32::max);
                worst = worst.max(off);
                let shown = |c: [f32; 3]| c.map(|c| c.clamp(0., 1.) * 255.);
                let apart = |a: [f32; 3]| (0..3).any(|c| (a[c] - want[c]).abs() > 10.);
                let unfogged = reference(&profile, sun, eye, anchor, at, look, false);
                let [glow, mask, ao, scale] = look;
                let without = [
                    shown(unfogged),
                    shown(reference(
                        &profile,
                        sun,
                        eye,
                        anchor,
                        at,
                        [0., mask, ao, scale],
                        true,
                    )),
                    shown(reference(
                        &profile,
                        sun,
                        eye,
                        anchor,
                        at,
                        [glow, 0., ao, scale],
                        true,
                    )),
                    shown(reference(
                        &profile,
                        sun,
                        eye,
                        anchor,
                        at,
                        [glow, mask, 1., scale],
                        true,
                    )),
                ];
                for (n, w) in moved.iter_mut().zip(without) {
                    *n += u32::from(apart(w));
                }
                assert!(
                    off <= 3.,
                    "sun {sun:?}, {distance:.0} m, look {look:?}: drew {got:?}, 2070 shows {want:?}"
                );
            }
        }
        eprintln!("worst {worst} levels; fog, glow, spec, AO moved {moved:?} of 72");
        assert!(moved.iter().all(|&n| n >= 4), "{moved:?}");
    }
}
