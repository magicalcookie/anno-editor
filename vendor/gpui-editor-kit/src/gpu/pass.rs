// Owns how a scene plugs in drawing of its own (a game's water, sky,
// ground): the frame's fixed stages, what a plugged pass prepares and
// records with, and the shader prelude its pipelines include
// (gpu/frame.wgsl).
/// Bind group 0 and the shading helpers every scene pass shares, with the
/// kit's distance fog as `fog`. A plugged pass's WGSL is this followed by
/// its own source, declaring its bindings from group 1 up. A pass drawn
/// under a game's fog (`gpu::Lighting`) starts from its family set's
/// [`super::Families::pass_prelude`] instead.
pub const FRAME_WGSL: &str = concat!(include_str!("frame.wgsl"), kit_fog!());

/// `half`, `half2`, `half3` and `half4` for shading math that fits 16 bits
/// (colours, blend weights, unit vectors): WGSL's f16 where `device` has
/// it, f32 elsewhere, so one source serves both. Apple's GPUs run f16 at
/// twice f32's rate and hold it in half the registers. A module's first
/// text: WGSL takes `enable` directives before anything else. Convert with
/// `half(x)` / `half4(v)` and back with `f32(x)` / `vec4<f32>(v)`.
pub fn half_types(device: &wgpu::Device) -> &'static str {
    if device.features().contains(wgpu::Features::SHADER_F16) {
        "enable f16;\nalias half = f16;\nalias half2 = vec2<f16>;\nalias half3 = vec3<f16>;\nalias half4 = vec4<f16>;\n"
    } else {
        "alias half = f32;\nalias half2 = vec2<f32>;\nalias half3 = vec3<f32>;\nalias half4 = vec4<f32>;\n"
    }
}

/// frame.wgsl alone: the kit's own modules end with their family set's
/// `shade` and `fog` (`Families::scene`).
pub(crate) const FRAME_PRELUDE: &str = include_str!("frame.wgsl");

/// Screen-space reflections (gpu/ssr.wgsl) for a pass drawn after the
/// opaque scene: `ssr_trace(scene, origin, dir, ssr, shift)` returns the
/// opaque color a reflected ray meets on screen (read `shift` pixels off
/// it), and how far to trust it, from the
/// frame's depth and the `Record::scene` copy the pass binds. Append after
/// [`FRAME_WGSL`]; [`Ssr::params`] fills its `Ssr`. `ssr_hit` and
/// `ssr_color` split the plain trace, for a pass that reads the hit
/// elsewhere; [`ssr_module`]'s quad trace has no such split.
pub const SSR_WGSL: &str = include_str!("ssr.wgsl");

/// A plugged pass's module source that traces reflections: the frame
/// prelude, [`SSR_WGSL`], then `own`. Where the device has quad operations
/// ([`wgpu::Features::SUBGROUP`]) `ssr_trace` is gpu/ssr_quad.wgsl's
/// instead, which traces each 2x2 pixel quad's ray together (call it in
/// the same control flow for the whole quad).
pub fn ssr_module(device: &wgpu::Device, own: &[&str]) -> String {
    let mut source = String::new();
    // Naga takes quad operations by the device's capabilities; it rejects
    // WGSL's `enable subgroups` as not yet implemented.
    if device.features().contains(wgpu::Features::SUBGROUP) {
        source.push_str(FRAME_WGSL);
        source.push_str(&SSR_WGSL.replace("fn ssr_trace(", "fn ssr_trace_plain("));
        source.push_str(include_str!("ssr_quad.wgsl"));
    } else {
        source.push_str(FRAME_WGSL);
        source.push_str(SSR_WGSL);
    }
    for part in own {
        source.push_str(part);
    }
    source
}

/// How a pass traces [`SSR_WGSL`]'s reflections: a game's own reach and
/// cost.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Ssr {
    /// Depth tests along the ray before it gives up; steps grow along it.
    /// 0 traces nothing: the caller's own reflection (the sky) alone.
    pub steps: u32,
    /// How far, in world units, a ray looks.
    pub reach: f32,
    /// How deep behind a surface, in world units, a ray still counts as
    /// meeting it rather than passing behind it.
    pub thickness: f32,
    /// How far in from the screen's edges reflections fade out, as a share
    /// of the viewport: what lies past the edge cannot be reflected.
    pub edge: f32,
}

impl Ssr {
    /// The WGSL `Ssr`'s `params`.
    pub fn params(&self) -> [f32; 4] {
        [self.steps as f32, self.reach, self.thickness, self.edge]
    }
}

/// The scene's color target: linear radiance in a toned scene, display
/// values otherwise, with coverage in alpha. The post stage tone maps it.
pub const HDR_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba16Float;

/// Every scene pass's depth: reversed, 1 near and 0 far, so passes test with
/// `GreaterEqual`.
pub const DEPTH_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Depth32Float;

/// The sun's shadow map and the views' cascades. Sixteen bits step about
/// 5 mm over a 300 m caster range, well under the half-texel bias, and
/// halve the maps (68 MB at the high preset).
pub const SHADOW_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Depth16Unorm;

/// The Opaque stage's color targets, in location order: the HDR color, the
/// pick handle (`R32Uint`; handle 0 picks nothing), and the linear color
/// water refracts (RGB9E5 in `R32Uint`). The prelude's `Opaque` output and
/// `shaded()` write all three.
pub const OPAQUE_TARGETS: [wgpu::TextureFormat; 3] = [
    HDR_FORMAT,
    wgpu::TextureFormat::R32Uint,
    wgpu::TextureFormat::R32Uint,
];

/// A frame's stages, in the order they run. The kit draws its own work in
/// each; plugged passes record into the five marked as open to them, after
/// the kit's work there, and any of them can also cast into the shadow
/// map ([`Pass::shadow`]). At Opaque, Water and Sky a pass can draw instead
/// of the kit's own ground, far sea or sky backdrop ([`Pass::replaces`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Stage {
    /// The sun's shadow map, drawn again only when what casts into it
    /// changed: the kit's casters, then each ready pass with a
    /// [`Pass::shadow`] through [`Pass::record_shadow`]. Depth only
    /// ([`SHADOW_FORMAT`]), cleared to 1 with nearer smaller (`LessEqual`):
    /// not reversed like the scene's depth. Group 0's camera is the sun's,
    /// so `globals.clip` takes world positions into the map and
    /// `globals.eye` is the direction toward the sun (w 0).
    Shadow,
    /// Open to passes: meshes and placements into the HDR target, with
    /// pick handles and the linear copy [`Record::scene`] reads; a pass
    /// writes all of [`OPAQUE_TARGETS`] and depth. The ground is a pass
    /// here too: the kit's [`TerrainPass`], listed first, draws
    /// [`Scene::terrain`] and casts its shadow. A ready pass that replaces
    /// takes its place, ground and shadow both; the terrain still supplies
    /// the heights and colors placements drape on and take their tint
    /// from. [`Record::scene`] is not yet written here.
    ///
    /// [`Scene::terrain`]: super::Scene::terrain
    /// [`TerrainPass`]: super::TerrainPass
    Opaque,
    /// Screen-space ambient occlusion over the opaque scene.
    Occlusion,
    /// Open to passes: opaque, depth-written surfaces that see the opaque
    /// scene (water). HDR target and depth; group 0's depth map is the
    /// opaque depth. The kit draws its far sea first, where the scene's
    /// sky has one ([`Sky::sea`]): a plane at sea level where nothing
    /// opaque drew, which writes no depth, so the backdrop leaves it be
    /// and a game's sky that replaces the backdrop draws over it. A ready
    /// pass that replaces draws its own sea instead.
    ///
    /// [`Sky::sea`]: super::Sky::sea
    Water,
    /// Open to passes: where depth is still clear, the backdrop. HDR target
    /// and depth, tested but not written. A pass that replaces draws the
    /// whole backdrop in place of the kit's (the sky map or gradient, and
    /// under the horizon its colour where the kit's sea does not lie);
    /// [`Scene::sky`] still lights the scene.
    ///
    /// [`Scene::sky`]: super::Scene::sky
    Sky,
    /// Open to passes, after the kit's decals and blended meshes. HDR target
    /// and depth; group 0's depth map is the opaque depth, copied before
    /// the Water stage (what lies under water, not its surface), for
    /// passes that shade what lies behind (haze).
    Blend,
    /// The HDR target tone mapped onto the viewport's own target.
    Post,
    /// Open to passes: display values over the toned image (lines,
    /// markers, outlines), no depth. The tone curve does not touch them, so
    /// their colors stay as given.
    Overlay,
}

/// What a pass gets once per view before the frame's passes record.
pub struct Prepare<'a> {
    pub device: &'a wgpu::Device,
    pub queue: &'a wgpu::Queue,
    /// For uploads and compute work that must land before drawing (a
    /// simulation step).
    pub encoder: &'a mut wgpu::CommandEncoder,
    /// Bind group 0's layout: every scene pipeline's first group.
    pub frame_layout: &'a wgpu::BindGroupLayout,
    /// Seconds on the renderer's clock, for passes that animate.
    pub time: f32,
    /// The clock cloud shadows move by (`Detail::cloud_shadows`): `time`,
    /// or `time` in steps; `None` when none are drawn, so a pass's
    /// [`Pass::cloud_shade`] is not used and need not be drawn.
    pub cloud_time: Option<f32>,
    /// Where the camera looks, world X and Z: the centre for detail that
    /// follows the view.
    pub focus: [f32; 2],
    /// The eye's world position in perspective, `None` in orthographic:
    /// for passes that bake what the camera sees from where it stands
    /// (a sky seen from its height).
    pub eye: Option<[f32; 3]>,
    /// The view's clip-to-world matrix (the prelude's `globals.unclip`,
    /// columns) and the target's size in pixels: for passes that render
    /// at their own resolution what the view's rays see.
    pub unclip: [[f32; 4]; 4],
    pub size: [u32; 2],
    /// Whether this frame is a window's, whose uploads keep to the time
    /// budget: `Budget::frame(timed)`. Offscreen frames upload by bytes alone.
    pub timed: bool,
    /// What the kit's own passes (its terrain) prepare with beyond the
    /// public handles; `None` outside a viewport's frame.
    pub(crate) kit: Option<super::KitPrepare<'a>>,
}

/// What a pass gets after the opaque scene, to draw into targets of its own
/// ([`Pass::after_opaque`]).
pub struct AfterOpaque<'a> {
    pub device: &'a wgpu::Device,
    pub queue: &'a wgpu::Queue,
    pub encoder: &'a mut wgpu::CommandEncoder,
    /// Bind group 0 as the Blend stage binds it: `t_depth` is the opaque
    /// depth's copy.
    pub frame: &'a wgpu::BindGroup,
    /// The view's target in pixels, and the viewport in it: x, y, width
    /// and height, as the stages' scissor.
    pub size: [u32; 2],
    pub viewport: [u32; 4],
    pub(crate) timing: Option<&'a mut super::Timing>,
}

impl AfterOpaque<'_> {
    /// The encoder, and timestamp writes for one render pass recorded with
    /// it under `label` when the frame is timed.
    pub fn timed(
        &mut self,
        label: &'static str,
    ) -> (
        &mut wgpu::CommandEncoder,
        Option<wgpu::RenderPassTimestampWrites<'_>>,
    ) {
        let writes = self.timing.as_mut().and_then(|t| t.render(label));
        (&mut *self.encoder, writes)
    }
}

/// What a pass gets to record with, per view.
pub struct Record<'a> {
    pub device: &'a wgpu::Device,
    pub queue: &'a wgpu::Queue,
    /// The opaque stage's linear color, one RGB9E5 `u32` per pixel
    /// (`unpack_rgb9e5` in the prelude): what water refracts.
    pub scene: &'a wgpu::TextureView,
    /// At Water and Blend: whether the kit handed the stage the ambient
    /// occlusion to multiply in (Pass::applies_occlusion), and left out its
    /// own pass for it. Else the prelude's `occlusion_factor` reads 1.
    pub occlusion: bool,
    /// What the kit's own passes record with beyond the public handles.
    pub(crate) kit: Option<super::KitRecord<'a>>,
}

/// What a pass casts into the sun's shadow map ([`Pass::shadow`]).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Caster {
    /// Changes whenever what the pass casts moved or changed: the map is
    /// drawn again only then, and not for a camera move. A pass whose
    /// casters animate gives a new version every frame.
    pub version: u64,
    /// World bounds of what it casts. The map grows to cover them, so
    /// casters outside the kit's own scene still land in it.
    pub bounds: [[f32; 3]; 2],
}

/// The light clouds leave near the view, as a pass draws it on the GPU
/// ([`Pass::cloud_shade`]): `view`'s red channel, 1 lit, over a world square
/// from `origin` (X, Z) `size` across, for points at sea level. Higher
/// points look it up where the line toward the sun meets sea level, so
/// the map holds for anything below the clouds. Inside the square it
/// stands in for the scene's [`super::CloudShadows`], handing over to them
/// across its outer tenth.
#[derive(Clone, Debug)]
pub struct CloudShade {
    pub view: wgpu::TextureView,
    pub origin: [f32; 2],
    pub size: f32,
    /// Changes with every redraw of `view`: a still view keeps its shaded
    /// layers only while it holds (gpu.rs `LayerKey`).
    pub version: u64,
}

/// Drawing a scene plugs in. Passes keep their GPU state inside (built on
/// the first prepare) behind their own lock, so the scene can hand the same
/// pass to every view and keep it across scenes. Keep it in an
/// [`OnDevice`](super::OnDevice): after a device loss the next frame comes
/// on a new device, and the pass builds its state again there.
pub trait Pass: Send + Sync {
    /// Opaque, Water, Sky, Blend or Overlay; the kit skips a pass naming
    /// another.
    fn stage(&self) -> Stage;
    /// Uploads and compute work before this view's frame. False skips its
    /// drawing this frame (still loading) and keeps frames coming.
    fn prepare(&self, frame: &mut Prepare<'_>) -> bool;
    /// Draws into the stage's render pass. Group 0 is bound and the
    /// scissor set; the pass binds its pipeline and its own groups.
    fn record(&self, frame: &Record<'_>, pass: &mut wgpu::RenderPass<'_>);
    /// Whether it changes by itself, so the view keeps drawing.
    fn animating(&self) -> bool {
        false
    }
    /// At Opaque, Water or Sky: draws instead of the kit's terrain pass
    /// (its ground and its shadow), far sea or sky backdrop, rather than
    /// over them. Only while the pass is ready: one still loading leaves
    /// the kit's in place.
    fn replaces(&self) -> bool {
        false
    }
    /// At Opaque: changes whenever what the pass draws would, beyond what
    /// the kit keys a view's opaque layers on itself (the camera, the
    /// scene, its edits and uploads, the lights; gpu.rs `LayerKey`). While
    /// it and they hold still, a still view keeps the layers instead of
    /// drawing them again. `None`, the default, draws the pass every frame
    /// and so keeps nothing: a pass that animates or cannot tell.
    fn version(&self) -> Option<u64> {
        None
    }
    /// Its shadow casters, once prepared; `None` casts nothing.
    fn shadow(&self) -> Option<Caster> {
        None
    }
    /// The cloud shade it drew for this view, once prepared: the first
    /// ready pass with one lights the frame by it.
    fn cloud_shade(&self) -> Option<CloudShade> {
        None
    }
    /// At Blend, once prepared: whether it multiplies the ambient occlusion
    /// over the opaque scene itself, as it draws over every pixel (the
    /// prelude's `occlusion_factor`), so the kit leaves out its own full-
    /// screen pass for it. Only where the device blends with two sources:
    /// the factor is a color, the pass's own output another.
    fn applies_occlusion(&self) -> bool {
        false
    }
    /// Draws into the pass's own targets once the opaque scene is drawn and
    /// its depth copied, before the Water stage: for work over that depth
    /// at a resolution of its own (haze at half size), which its stage then
    /// composites. Called each frame the depth is copied (when any Water or
    /// Blend pass, occlusion or blended mesh is drawn) for every ready pass.
    fn after_opaque(&self, _frame: &mut AfterOpaque<'_>) {}
    /// Draws its casters into the shadow map ([`Stage::Shadow`]): called
    /// only when the map is drawn again, after the kit's casters. Group 0
    /// is bound; [`Record::scene`] is not written yet.
    fn record_shadow(&self, _frame: &Record<'_>, _pass: &mut wgpu::RenderPass<'_>) {}
    /// Compiles what the pass builds once per device (`super::per_device`),
    /// on the calling thread, so its `prepare` then finds it built. An
    /// offscreen settle calls it before its clock starts
    /// (`Offscreen::render`); a window's host warms through
    /// [`super::warm`]'s hook instead, before a view needs the pass. Cheap
    /// once built.
    fn warm(&self, _gpu: &super::Warm<'_>) {}
}
