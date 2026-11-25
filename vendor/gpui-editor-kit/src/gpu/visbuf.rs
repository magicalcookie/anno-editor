// Owns the visibility buffer, the one way lit placements reach the screen:
// its passes and pipelines (visbuf.wgsl), which shade with placed.wgsl's
// `placed_lit`.
use super::{Device, Kit, Targets, entry, layout_entry, placed, texture_2d};

/// Storage buffers the resolve binds: the frame's markers and lines,
/// placed.wgsl's four per-view arrays (the instance words the fourth) and
/// the arena's eight (its index pages the eighth). Every desktop adapter
/// wgpu reports has at least 16 (Vulkan's and D3D12's floor is far above;
/// Metal's is 31); a device held to the default limit, 8, draws no 3D
/// (`super::supported`).
pub(super) const RESOLVE_STORAGE_BUFFERS: u32 = 14;

/// What the visibility pass needs to hash each pixel's triangle's corners:
/// fragments reading their corners' values (visbuf_corners.wgsl).
pub(crate) const CORNER_FEATURES: wgpu::Features = wgpu::Features::SHADER_PER_VERTEX;

/// The side in pixels of the screen tiles posed placements mark in the
/// cull (cull.wgsl `mark_posed`), which the posed resolve keeps to.
pub(super) const POSED_TILE: u32 = 64;

/// Words of `Targets::posed_tiles`: four of flags, then a bit a tile for a
/// target up to 8192 pixels each way (visbuf.wgsl POSED_VECTORS).
pub(super) const POSED_WORDS: u64 = 4 + (8192 / POSED_TILE as u64).pow(2) / 32;

/// Triangle-table places a hashed pixel holds (24 bits, less the empty 0):
/// an arena with more draws without the hash.
const CORNER_TABLES: u32 = (1 << 24) - 1;

/// The depth each visibility buffer pixel was drawn at (visbuf.wgsl
/// VisWritten), beside the buffer.
const VIS_DEPTH_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::R32Float;

/// The buffer and its depth sit after the opaque targets
/// (`super::OPAQUE_TARGETS`) in every visibility pass (visbuf.wgsl
/// VisWritten's locations): the second cull phase draws lit placements into
/// the buffer and flat ones into the opaque targets in one render pass
/// (`Device::draw_revealed`), which a tile GPU would otherwise store and
/// load again between. The passes of the buffer alone leave the first three
/// slots empty.
pub(super) const VIS_TARGETS: [wgpu::TextureFormat; 2] =
    [wgpu::TextureFormat::Rg32Uint, VIS_DEPTH_FORMAT];

/// Colour attachments of the revealed pass: the opaque targets and the
/// buffer. Bytes a pixel, 8 + 4 + 4 + 8 + 4 = 28, under WebGPU's floor of
/// 32 (`max_color_attachment_bytes_per_sample`), so every device the kit
/// draws on takes the pass (`super::supported`).
pub(super) const REVEALED_ATTACHMENTS: u32 = 5;
pub(super) const REVEALED_BYTES: u32 = 28;

/// The visibility buffer's pipelines: lit placements into it, and the
/// resolve out of it, with the resolve's layout for group 1.
#[derive(Clone)]
pub(super) struct Visibility {
    /// Per pass (0 the buffer's own, 1 the revealed pass with the opaque
    /// targets beside it): placements their family cuts out, then those
    /// nothing cuts out (`Ranges::lit_opaque`).
    plain: [[wgpu::RenderPipeline; 2]; 2],
    /// The same, hashing their triangles' corners where the device has
    /// per-vertex fragment inputs (visbuf_corners.wgsl).
    corners: Option<[[wgpu::RenderPipeline; 2]; 2]>,
    /// The resolve (visbuf.wgsl cs_resolve), with its group 1, and the
    /// posed placements' (cs_resolve_posed).
    resolve: (wgpu::BindGroupLayout, [wgpu::ComputePipeline; 2]),
}

impl Visibility {
    /// The pipelines over `families`' modules, where the device has the
    /// placement arena.
    pub fn new(device: &wgpu::Device, kit: &Kit, families: &placed::Families) -> Option<Self> {
        let (arena, _) = kit.arena.as_ref()?;
        let corners = device.features().contains(CORNER_FEATURES);
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("editor visibility buffer"),
            source: wgpu::ShaderSource::Wgsl(
                (families.module(placed::Module::Visibility { corners })).into(),
            ),
        });
        let pipeline_layout = |label, group1: Option<&wgpu::BindGroupLayout>| {
            device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some(label),
                bind_group_layouts: &[
                    Some(&kit.frame_layout),
                    group1,
                    Some(&kit.placed_layout),
                    Some(arena),
                ],
                immediate_size: 0,
            })
        };
        let target = |format, write_mask| {
            Some(wgpu::ColorTargetState {
                format,
                blend: None,
                write_mask,
            })
        };
        // The buffer alone: pick handles cost 0.18 ms more here (Albion at
        // 5K), written per fragment instead of per pixel. In the revealed
        // pass the opaque targets are there, unwritten.
        let targets = [false, true].map(|revealed| {
            let opaque = (super::OPAQUE_TARGETS).map(|format| {
                revealed
                    .then(|| target(format, wgpu::ColorWrites::empty()))
                    .flatten()
            });
            let vis = VIS_TARGETS.map(|format| target(format, wgpu::ColorWrites::ALL));
            [
                opaque[0].clone(),
                opaque[1].clone(),
                opaque[2].clone(),
                vis[0].clone(),
                vis[1].clone(),
            ]
        });
        let geometry_layout = pipeline_layout("editor visibility", None);
        let geometry = |label: &str,
                        vertex: &str,
                        fragment: &str,
                        targets: &[Option<wgpu::ColorTargetState>]| {
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some(label),
                layout: Some(&geometry_layout),
                vertex: wgpu::VertexState {
                    module: &module,
                    entry_point: Some(vertex),
                    buffers: &[],
                    compilation_options: Default::default(),
                },
                fragment: Some(wgpu::FragmentState {
                    module: &module,
                    entry_point: Some(fragment),
                    targets,
                    compilation_options: Default::default(),
                }),
                primitive: wgpu::PrimitiveState::default(),
                // Reversed depth, last of equals wins, as every opaque
                // pipeline tests.
                depth_stencil: Some(wgpu::DepthStencilState {
                    format: wgpu::TextureFormat::Depth32Float,
                    depth_write_enabled: Some(true),
                    depth_compare: Some(wgpu::CompareFunction::GreaterEqual),
                    stencil: Default::default(),
                    bias: Default::default(),
                }),
                multisample: Default::default(),
                multiview_mask: None,
                cache: None,
            })
        };
        let pair = |[cutout, opaque]: [&str; 2]| {
            [0, 1].map(|pass| {
                let label = ["editor visibility", "editor visibility, revealed"][pass];
                [
                    geometry(label, "vs_vis", cutout, &targets[pass]),
                    geometry(label, "vs_vis_opaque", opaque, &targets[pass]),
                ]
            })
        };
        let plain = pair(["fs_vis", "fs_vis_opaque"]);
        // Into the opaque targets, after the opaque pass. In compute: as a
        // full-screen fragment pass the resolve cost 0.37, 0.71 and 0.82 ms on
        // continental_overview, pirate and n_m02 at 5K (RTX 4080 SUPER), in
        // compute 0.27, 0.51 and 0.71, per pixel the same work.
        let stored = |binding, format| {
            layout_entry(
                binding,
                wgpu::ShaderStages::COMPUTE,
                wgpu::BindingType::StorageTexture {
                    access: wgpu::StorageTextureAccess::WriteOnly,
                    format,
                    view_dimension: wgpu::TextureViewDimension::D2,
                },
            )
        };
        let compute_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("editor compute resolve"),
            entries: &[
                layout_entry(
                    0,
                    wgpu::ShaderStages::COMPUTE,
                    wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Uint,
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                ),
                layout_entry(
                    1,
                    wgpu::ShaderStages::COMPUTE,
                    wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: false },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                ),
                layout_entry(2, wgpu::ShaderStages::COMPUTE, super::depth_texture()),
                stored(3, super::HDR_FORMAT),
                stored(4, wgpu::TextureFormat::R32Uint),
                stored(5, wgpu::TextureFormat::R32Uint),
                layout_entry(7, wgpu::ShaderStages::COMPUTE, super::uniform(false)),
                layout_entry(8, wgpu::ShaderStages::COMPUTE, super::uniform(false)),
            ],
        });
        let compute_pipeline_layout =
            pipeline_layout("editor compute resolve", Some(&compute_layout));
        let compute = ["cs_resolve", "cs_resolve_posed"].map(|entry| {
            device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
                label: Some("editor compute resolve"),
                layout: Some(&compute_pipeline_layout),
                module: &module,
                entry_point: Some(entry),
                compilation_options: Default::default(),
                cache: None,
            })
        });
        let corners = corners.then(|| pair(["fs_vis_corners", "fs_vis_opaque_corners"]));
        Some(Self {
            plain,
            corners,
            resolve: (compute_layout, compute),
        })
    }
}

impl Targets {
    /// The visibility buffer's depth as it drew it, made on first use beside
    /// the buffer (`draw_visibility`).
    fn vis_depth(&mut self, gpu: &super::Gpu) -> wgpu::TextureView {
        let size = self.size;
        let (_, view) = self.vis_depth.get_or_insert_with(|| {
            texture_2d(
                gpu,
                "editor visibility depth",
                size,
                VIS_DEPTH_FORMAT,
                wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
            )
        });
        view.clone()
    }

    /// The visibility buffer, made on first use: views without placements
    /// never pay its 8 bytes a pixel.
    fn visibility(&mut self, gpu: &super::Gpu) -> wgpu::TextureView {
        let size = self.size;
        let (_, view) = self.vis.get_or_insert_with(|| {
            texture_2d(
                gpu,
                "editor visibility",
                size,
                wgpu::TextureFormat::Rg32Uint,
                wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
            )
        });
        view.clone()
    }

    /// The posed placements' own buffer and its depth
    /// (`Device::draw_posed`), made the first time a view has some.
    fn posed_visibility(&mut self, gpu: &super::Gpu) -> [wgpu::TextureView; 2] {
        let size = self.size;
        let usage = wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING;
        let [(_, vis), (_, depth)] = self.posed_vis.get_or_insert_with(|| {
            [
                texture_2d(
                    gpu,
                    "editor posed visibility",
                    size,
                    wgpu::TextureFormat::Rg32Uint,
                    usage,
                ),
                texture_2d(
                    gpu,
                    "editor posed visibility depth",
                    size,
                    VIS_DEPTH_FORMAT,
                    usage,
                ),
            ]
        });
        [vis.clone(), depth.clone()]
    }
}

impl Visibility {
    /// The pipelines `pass` draws with (0 the buffer's own, 1 the revealed
    /// pass), cutout and opaque: with the corners' hash while the arena's
    /// triangles fit its table.
    fn pipelines(&self, arena: &placed::Arena, pass: usize) -> &[wgpu::RenderPipeline; 2] {
        match &self.corners {
            Some(corners) if arena.tables < CORNER_TABLES => &corners[pass],
            _ => &self.plain[pass],
        }
    }
}

/// A colour attachment the pass loads (`load`) or clears, and stores.
fn attachment(view: &wgpu::TextureView, load: bool) -> Option<wgpu::RenderPassColorAttachment<'_>> {
    Some(wgpu::RenderPassColorAttachment {
        view,
        resolve_target: None,
        ops: wgpu::Operations {
            load: if load {
                wgpu::LoadOp::Load
            } else {
                wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT)
            },
            store: wgpu::StoreOp::Store,
        },
        depth_slice: None,
    })
}

impl Device {
    /// Lit placements the view saw last frame (the cull's first phase) into
    /// the visibility buffer, starting it and the frame's depth.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn draw_visibility(
        &self,
        gpu: &super::Gpu,
        encoder: &mut wgpu::CommandEncoder,
        targets: &mut Targets,
        placed: &placed::Placed,
        draws: &wgpu::BindGroup,
        frame: &wgpu::BindGroup,
        args: &wgpu::Buffer,
        scissor: [u32; 4],
    ) {
        let (Some(vis), Some(arena)) = (&self.visibility, &placed.arena) else {
            return;
        };
        let view = targets.visibility(gpu);
        let depth_view = targets.vis_depth(gpu);
        let timestamp_writes = targets.timing.as_mut().and_then(|t| t.render("visibility"));
        // y 0: no triangle (visbuf.wgsl), depth 0: none drew.
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("editor visibility"),
            color_attachments: &[
                None,
                None,
                None,
                attachment(&view, false),
                attachment(&depth_view, false),
            ],
            depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                view: &targets.depth.1,
                depth_ops: Some(wgpu::Operations {
                    // Reversed depth: 0 is the far plane.
                    load: wgpu::LoadOp::Clear(0.),
                    store: wgpu::StoreOp::Store,
                }),
                stencil_ops: None,
            }),
            timestamp_writes,
            occlusion_query_set: None,
            multiview_mask: None,
        });
        let [x, y, w, h] = scissor;
        pass.set_scissor_rect(x, y, w, h);
        pass.set_bind_group(0, frame, &[]);
        let r = &arena.ranges;
        self.draw_lit(
            &mut pass,
            vis.pipelines(arena, 0),
            arena,
            draws,
            args,
            [&r.lit_opaque, &r.lit_cutout],
        );
    }

    /// What the second cull phase revealed, after the opaque pass, in one
    /// render pass over the opaque targets, the buffer and depth: lit
    /// placements into the buffer, over the first phase's and over the
    /// ground, so the one resolve shades both phases; then flat meshes and
    /// impostors into the opaque targets, where the resolve then leaves
    /// what they hide.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn draw_revealed(
        &self,
        gpu: &super::Gpu,
        encoder: &mut wgpu::CommandEncoder,
        targets: &mut Targets,
        placed: &placed::Placed,
        draws: &wgpu::BindGroup,
        frame: &wgpu::BindGroup,
        args: &wgpu::Buffer,
        scissor: [u32; 4],
    ) {
        let (Some(vis), Some(arena)) = (&self.visibility, &placed.arena) else {
            return;
        };
        let view = targets.visibility(gpu);
        let depth_view = targets.vis_depth(gpu);
        let timestamp_writes = targets.timing.as_mut().and_then(|t| t.render("revealed"));
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("editor revealed"),
            color_attachments: &[
                attachment(&targets.hdr.1, true),
                attachment(&targets.ids.1, true),
                attachment(&targets.scene.1, true),
                attachment(&view, true),
                attachment(&depth_view, true),
            ],
            depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                view: &targets.depth.1,
                depth_ops: Some(wgpu::Operations {
                    load: wgpu::LoadOp::Load,
                    store: wgpu::StoreOp::Store,
                }),
                stencil_ops: None,
            }),
            timestamp_writes,
            occlusion_query_set: None,
            multiview_mask: None,
        });
        let [x, y, w, h] = scissor;
        pass.set_scissor_rect(x, y, w, h);
        pass.set_bind_group(0, frame, &[]);
        let r = &arena.ranges;
        self.draw_lit(
            &mut pass,
            vis.pipelines(arena, 1),
            arena,
            draws,
            args,
            [&r.lit_opaque, &r.lit_cutout],
        );
        self.draw_arena_opaque(&mut pass, placed, draws, args, true, true, None);
    }

    /// Lit arena draws into the buffer, `[whole, cut]` ranges: what nothing
    /// cuts out first, without its albedo sample.
    fn draw_lit(
        &self,
        pass: &mut wgpu::RenderPass<'_>,
        [cutout, opaque]: &[wgpu::RenderPipeline; 2],
        arena: &placed::Arena,
        draws: &wgpu::BindGroup,
        args: &wgpu::Buffer,
        [whole, cut]: [&[std::ops::Range<u32>]; 2],
    ) {
        let pass_ = super::ArenaPass::Visibility;
        self.draw_arena(pass_, pass, opaque, arena, draws, args, whole);
        self.draw_arena(pass_, pass, cutout, arena, draws, args, cut);
    }

    /// Shades the visibility buffer's placements into the opaque targets,
    /// pick handles too, after the opaque pass: over the ground and the
    /// decals on it, as the game draws its models after them (Anno1800.exe
    /// build 16367353 sub_141C389B0), but under whatever drew nearer. Each pixel compares
    /// the depth the buffer drew it at (`vis_depth`) with the depth target's
    /// and leaves where that holds a nearer surface, as the opaque pass's
    /// own placements would test. Over kept geometry (`Reuse::Geometry`)
    /// this is all lit placements cost.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn resolve(
        &self,
        gpu: &super::Gpu,
        encoder: &mut wgpu::CommandEncoder,
        targets: &mut Targets,
        placed: &placed::Placed,
        draws: &wgpu::BindGroup,
        frame: &wgpu::BindGroup,
        scissor: [u32; 4],
    ) {
        let view = targets.visibility(gpu);
        let depth = targets.vis_depth(gpu);
        self.resolve_from(
            gpu,
            encoder,
            targets,
            placed,
            [&view, &depth],
            draws,
            frame,
            scissor,
            false,
        );
    }

    /// The resolve over the buffer `from` (its triangles and their depth);
    /// `posed` only over the tiles the view's culls marked for posed
    /// placements (`Targets::posed_tiles`).
    #[allow(clippy::too_many_arguments)]
    fn resolve_from(
        &self,
        gpu: &super::Gpu,
        encoder: &mut wgpu::CommandEncoder,
        targets: &mut Targets,
        placed: &placed::Placed,
        [view, depth]: [&wgpu::TextureView; 2],
        draws: &wgpu::BindGroup,
        frame: &wgpu::BindGroup,
        scissor: [u32; 4],
        posed: bool,
    ) {
        let (Some(vis), Some(arena)) = (&self.visibility, &placed.arena) else {
            return;
        };
        let [x, y, w, h] = scissor;
        let rect = super::buffer_init(
            gpu,
            "editor compute resolve rect",
            bytemuck::cast_slice(&[x, y, w, h]),
            wgpu::BufferUsages::UNIFORM,
        );
        let group = gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("editor compute resolve"),
            layout: &vis.resolve.0,
            entries: &[
                entry(0, wgpu::BindingResource::TextureView(view)),
                entry(1, wgpu::BindingResource::TextureView(depth)),
                entry(2, wgpu::BindingResource::TextureView(&targets.depth.1)),
                entry(3, wgpu::BindingResource::TextureView(&targets.hdr.1)),
                entry(4, wgpu::BindingResource::TextureView(&targets.ids.1)),
                entry(5, wgpu::BindingResource::TextureView(&targets.scene.1)),
                entry(7, rect.as_entire_binding()),
                entry(8, targets.posed_tiles.as_entire_binding()),
            ],
        });
        let timestamp_writes = targets.timing.as_mut().and_then(|t| t.compute("resolve"));
        let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
            label: Some("editor compute resolve"),
            timestamp_writes,
        });
        pass.set_pipeline(&vis.resolve.1[usize::from(posed)]);
        pass.set_bind_group(0, frame, &[]);
        pass.set_bind_group(1, &group, &[]);
        pass.set_bind_group(2, draws, &[]);
        pass.set_bind_group(3, &arena.group, &[]);
        if posed {
            // A workgroup a tile the scissor touches.
            let tiles = |at: u32, size: u32| (at + size).div_ceil(POSED_TILE) - at / POSED_TILE;
            pass.dispatch_workgroups(tiles(x, w), tiles(y, h), 1);
        } else {
            pass.dispatch_workgroups(w.div_ceil(8), h.div_ceil(8), 1);
        }
    }

    /// Posed placements (`Ranges::posed`) over the opaque targets as they
    /// stand, after the layers a view keeps, so a view at rest draws these
    /// alone again: into a buffer of their own, depth tested against the
    /// frame's depth, which they write, then resolved. Not into the main
    /// buffer, which kept geometry keeps and which would then hold last
    /// frame's poses. After the occlusion stage: a kept frame holds no
    /// posed surface to darken. The lists of both cull phases.
    pub(super) fn draw_posed(
        &self,
        gpu: &super::Gpu,
        encoder: &mut wgpu::CommandEncoder,
        placed: &placed::Placed,
        targets: &mut Targets,
        frame: &wgpu::BindGroup,
        scissor: [u32; 4],
    ) {
        let (Some(vis), Some(arena), Some(buffers), Some(cull)) = (
            &self.visibility,
            &placed.arena,
            &placed.buffers,
            &targets.cull,
        ) else {
            return;
        };
        let phases = [
            (
                self.arena_draws(gpu, buffers, &cull.visible, &cull.keep),
                cull.args.clone(),
            ),
            (
                self.arena_draws(gpu, buffers, &cull.revealed_visible, &cull.keep),
                cull.revealed_args.clone(),
            ),
        ];
        let [view, depth_view] = targets.posed_visibility(gpu);
        let timestamp_writes = targets.timing.as_mut().and_then(|t| t.render("posed"));
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("editor posed visibility"),
            color_attachments: &[
                None,
                None,
                None,
                attachment(&view, false),
                attachment(&depth_view, false),
            ],
            depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                view: &targets.depth.1,
                depth_ops: Some(wgpu::Operations {
                    load: wgpu::LoadOp::Load,
                    store: wgpu::StoreOp::Store,
                }),
                stencil_ops: None,
            }),
            timestamp_writes,
            occlusion_query_set: None,
            multiview_mask: None,
        });
        let [x, y, w, h] = scissor;
        pass.set_scissor_rect(x, y, w, h);
        pass.set_bind_group(0, frame, &[]);
        let r = &arena.ranges;
        let pipelines = vis.pipelines(arena, 0);
        for (draws, args) in &phases {
            self.draw_lit(
                &mut pass,
                pipelines,
                arena,
                draws,
                args,
                [&r.posed_whole, &r.posed_cutout],
            );
        }
        drop(pass);
        let draws = &phases[0].0;
        self.resolve_from(
            gpu,
            encoder,
            targets,
            placed,
            [&view, &depth_view],
            draws,
            frame,
            scissor,
            true,
        );
    }
}
