// Owns rivers and lakes as Anno 1800 draws them (water_inland_water.fx
// psRiver/psLake) on the editor's water lattice (core::water::Inland):
// each record's colour and turbidity, its rivers' small waves and foam
// carried along the flow map, its lakes rippled by the ocean's small waves.
// Drawn in the sea's pass after the sea, with its bindings 0 to 10
// (water::Surface), so it needs a sea.
// Engine addresses here are Anno1800.exe build 16367353 (Steam).
use super::{entry, layout_entry, storage, uniform};
use anno_core::water::{Inland, Look};
use anno_formats::vfs::Install;
use gpui_editor_kit::texture::{Dds, Texture};
use std::sync::Arc;

/// `WaterColor` where a record leaves it out: 0xff0a5596, the default
/// River's and Lake's serializers (Anno1800.exe 0x140da60ba, 0x14105e02a)
/// compare against, an ARGB blue.
const DEFAULT_COLOR: u32 = 0xff0a_5596;
/// `Turbidity` where a River record leaves it out: the serializer writes
/// it only when it differs from 1.0 (0x14550d868, 0x140da60f7). Lake
/// records have no turbidity; they take the same.
const DEFAULT_TURBIDITY: f32 = 1.;

/// A `River` or `Lake` record as 1800 shades it: cInlandWaterColor and
/// cInlandWaterTurbidity. Its `RenderFlags` are the lattice's: corners
/// carry the fade (river::inland).
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct Record {
    pub color: [f32; 3],
    pub turbidity: f32,
}

impl Record {
    pub fn read(look: &Look) -> Self {
        // Packed ARGB stored in the float field's bits. TODO: whether the
        // engine linearises it before the shader was not traced; the bytes
        // go over 255 as they are.
        let argb = (look.field("WaterColor").and_then(|v| v.first()))
            .map_or(DEFAULT_COLOR, |v| v.to_bits());
        let byte = |shift: u32| ((argb >> shift) & 0xff) as f32 / 255.;
        Self {
            color: [byte(16), byte(8), byte(0)],
            turbidity: look.number("Turbidity").unwrap_or(DEFAULT_TURBIDITY),
        }
    }
}

/// The textures the water samples beside its waves: cWaterFoamMap
/// (`foam01`, each channel a different foam), cNoiseTex
/// (`noise_tex_3_16`) and cNoiseTex2 (`noise01`), as the renderer loads
/// them (Anno1800.exe 0x141ec35b9, 0x141e6672b, 0x141e6679a; the noise
/// textures bind to their names in the order they load), mip chains;
/// cNoiseTex3D (`noise_3d`, 64³ bytes, its first mip); and the lakes'
/// cWaterDuckweedMap and cWaterDuckweedNormMap.
pub(super) struct Textures {
    pub foam: Vec<Texture>,
    pub noise: Vec<Texture>,
    pub noise2: Vec<Texture>,
    pub noise3d: Vec<u8>,
    duckweed: Vec<Texture>,
    duckweed_norm: Vec<Texture>,
}

/// Texels a side of `noise_3d.dds`.
pub(super) const NOISE_3D: u32 = 64;

impl Textures {
    pub fn read(install: &Install) -> Option<Self> {
        let read = |path: &str| -> Option<Vec<Texture>> {
            let bytes = (install.read(path))
                .map_err(|e| eprintln!("1800 inland water: {path}: {e:#}"))
                .ok()?;
            let dds = Dds::parse(&bytes)
                .map_err(|e| eprintln!("1800 inland water: {path}: {e:#}"))
                .ok()?;
            (0..dds.mips.max(1))
                .map(|m| dds.decode(&bytes, m))
                .collect::<anyhow::Result<Vec<_>>>()
                .map_err(|e| eprintln!("1800 inland water: {path}: {e:#}"))
                .ok()
        };
        // An uncompressed 8-bit volume: a 128-byte header, then the mips.
        let noise3d = {
            let path = "data/graphics/effects/noise/noise_3d.dds";
            let bytes = (install.read(path))
                .map_err(|e| eprintln!("1800 inland water: {path}: {e:#}"))
                .ok()?;
            let texels = NOISE_3D.pow(3) as usize;
            let u32_at = |at: usize| u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap());
            let ok = bytes.len() >= 128 + texels
                && &bytes[..4] == b"DDS "
                && [u32_at(12), u32_at(16), u32_at(24)] == [NOISE_3D; 3]
                && u32_at(88) == 8;
            if !ok {
                eprintln!("1800 inland water: {path}: not a 64³ 8-bit volume");
                return None;
            }
            bytes[128..128 + texels].to_vec()
        };
        Some(Self {
            foam: read("data/graphics/effects/water/foam01_0.dds")?,
            noise: read("data/graphics/effects/noise/noise_tex_3_16_0.dds")?,
            noise2: read("data/graphics/effects/noise/noise01_0.dds")?,
            noise3d,
            duckweed: read("data/graphics/effects/water/duckweed_0.dds")?,
            duckweed_norm: read("data/graphics/effects/water/duckweed_norm_0.dds")?,
        })
    }
}

#[cfg(test)]
impl Textures {
    /// Foam in stripes along X in every channel, flat noise: what moves on
    /// a river is its foam.
    pub fn striped() -> Self {
        let image = |side: u32, texel: &dyn Fn(u32, u32) -> u8| Texture {
            width: side,
            height: side,
            rgba: (0..side * side)
                .flat_map(|i| {
                    let v = texel(i % side, i / side);
                    [v, v, v, 255]
                })
                .collect(),
        };
        Self {
            foam: vec![image(64, &|x, _| if x % 16 < 8 { 255 } else { 0 })],
            noise: vec![image(16, &|_, _| 128)],
            noise2: vec![image(16, &|_, _| 128)],
            noise3d: vec![128; NOISE_3D.pow(3) as usize],
            duckweed: vec![image(16, &|_, _| 0)],
            duckweed_norm: vec![image(16, &|_, _| 128)],
        }
    }
}

/// Each look's uniforms (inland.wgsl's InlandLook): colour, turbidity.
fn look_uniforms(inland: &Inland) -> Vec<[f32; 4]> {
    (inland.looks.iter())
        .map(|l| {
            let r = Record::read(l);
            [r.color[0], r.color[1], r.color[2], r.turbidity]
        })
        .collect()
}

/// The inland pass's GPU state: its pipelines (lakes and rivers on the
/// lattice, rivers from their strips), its textures and the lattice
/// uploaded.
pub(super) struct Pass {
    layout: wgpu::BindGroupLayout,
    lakes: wgpu::RenderPipeline,
    rivers: wgpu::RenderPipeline,
    river_strips: wgpu::RenderPipeline,
    params: wgpu::Buffer,
    duckweed: wgpu::TextureView,
    duckweed_norm: wgpu::TextureView,
    lattice: Option<Lattice>,
}

struct Lattice {
    key: usize,
    corners: wgpu::Buffer,
    looks: wgpu::Buffer,
    indices: wgpu::Buffer,
    split: u32,
    count: u32,
    /// Each river's own strip (`Inland::strips`): vertices, indices and
    /// how many. The vertices are a stand-in word without strips, so the
    /// bind group always has a buffer.
    strip_vertices: wgpu::Buffer,
    strip_indices: Option<(wgpu::Buffer, u32)>,
}

impl Pass {
    /// `shared` are the sea surface's layout entries (water::Surface).
    pub fn new(
        d: &wgpu::Device,
        queue: &wgpu::Queue,
        shader: &wgpu::ShaderModule,
        frame: &wgpu::BindGroupLayout,
        shared: &[wgpu::BindGroupLayoutEntry],
        textures: &Textures,
    ) -> Self {
        use wgpu::ShaderStages as S;
        let both = S::VERTEX | S::FRAGMENT;
        let texture = wgpu::BindingType::Texture {
            sample_type: wgpu::TextureSampleType::Float { filterable: true },
            view_dimension: wgpu::TextureViewDimension::D2,
            multisampled: false,
        };
        let mut entries = shared.to_vec();
        entries.extend([
            layout_entry(11, both, uniform()),
            layout_entry(12, both, storage(true)),
            layout_entry(13, S::FRAGMENT, storage(true)),
            layout_entry(30, S::VERTEX, storage(true)),
            layout_entry(21, S::FRAGMENT, texture),
            layout_entry(22, S::FRAGMENT, texture),
        ]);
        let layout = d.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("1800 inland water"),
            entries: &entries,
        });
        let pipeline_layout = d.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("1800 inland water"),
            bind_group_layouts: &[Some(frame), Some(&layout)],
            immediate_size: 0,
        });
        let pipeline = |vertex: &str, fragment: &str, blend: Option<wgpu::BlendState>| {
            d.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some(fragment),
                layout: Some(&pipeline_layout),
                vertex: wgpu::VertexState {
                    module: shader,
                    entry_point: Some(vertex),
                    buffers: &[],
                    compilation_options: Default::default(),
                },
                fragment: Some(wgpu::FragmentState {
                    module: shader,
                    entry_point: Some(fragment),
                    targets: &[Some(wgpu::ColorTargetState {
                        format: gpui_editor_kit::gpu::HDR_FORMAT,
                        blend,
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                    compilation_options: Default::default(),
                }),
                primitive: wgpu::PrimitiveState::default(),
                // Tested and written as the sea, so what is blended later
                // meets the water.
                depth_stencil: Some(wgpu::DepthStencilState {
                    format: gpui_editor_kit::gpu::DEPTH_FORMAT,
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
        // Once per device, as the sea's (`gpu::per_device`).
        use gpui_editor_kit::gpu::per_device;
        let lakes = per_device(d, "1800 lakes", || pipeline("vs_inland", "fs_lake", None));
        // Rivers fade in at their source and out at their end by alpha
        // (sbInlandWater's STANDARD_ALPHA_BLEND_STATES).
        let alpha = Some(wgpu::BlendState::ALPHA_BLENDING);
        let rivers = per_device(d, "1800 rivers", || {
            pipeline("vs_inland", "fs_river", alpha)
        });
        let river_strips = per_device(d, "1800 river strips", || {
            pipeline("vs_strip", "fs_river", alpha)
        });
        let params = d.create_buffer(&wgpu::BufferDescriptor {
            label: Some("1800 inland water"),
            size: 2 * 16,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        Self {
            layout,
            lakes,
            rivers,
            river_strips,
            params,
            duckweed: upload(d, queue, "1800 duckweed", &textures.duckweed),
            duckweed_norm: upload(d, queue, "1800 duckweed normals", &textures.duckweed_norm),
            lattice: None,
        }
    }

    /// Uploads the lattice when it changed (its flow map comes built,
    /// `Inland::gpu_corners`), and the grid's uniforms.
    pub fn prepare(&mut self, d: &wgpu::Device, queue: &wgpu::Queue, inland: &Arc<Inland>) {
        let key = Arc::as_ptr(inland) as usize;
        if self.lattice.as_ref().map(|l| l.key) != Some(key) {
            self.lattice = upload_lattice(d, inland, key);
        }
        let params = [
            [
                inland.origin[0],
                inland.origin[1],
                inland.step,
                1. / inland.step.max(1e-6),
            ],
            [inland.size[0] as f32, inland.size[1] as f32, 0., 0.],
        ];
        queue.write_buffer(&self.params, 0, bytemuck::cast_slice(&params));
    }

    /// Draws lakes, then rivers; `shared` are the sea's bind group entries.
    pub fn draw(
        &self,
        d: &wgpu::Device,
        pass: &mut wgpu::RenderPass<'_>,
        shared: Vec<wgpu::BindGroupEntry<'_>>,
    ) {
        let Some(lattice) = &self.lattice else {
            return;
        };
        let view = wgpu::BindingResource::TextureView;
        let mut entries = shared;
        entries.extend([
            entry(11, self.params.as_entire_binding()),
            entry(12, lattice.corners.as_entire_binding()),
            entry(13, lattice.looks.as_entire_binding()),
            entry(30, lattice.strip_vertices.as_entire_binding()),
            entry(21, view(&self.duckweed)),
            entry(22, view(&self.duckweed_norm)),
        ]);
        let group = d.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("1800 inland water"),
            layout: &self.layout,
            entries: &entries,
        });
        pass.set_bind_group(1, &group, &[]);
        pass.set_index_buffer(lattice.indices.slice(..), wgpu::IndexFormat::Uint32);
        if lattice.split > 0 {
            pass.set_pipeline(&self.lakes);
            pass.draw_indexed(0..lattice.split, 0, 0..1);
        }
        // Rivers from their own strips where the island built them, so a
        // pool above a waterfall and the river below never share cells;
        // the lattice's river cells then only carry the flow map.
        if let Some((indices, count)) = &lattice.strip_indices {
            pass.set_pipeline(&self.river_strips);
            pass.set_index_buffer(indices.slice(..), wgpu::IndexFormat::Uint32);
            pass.draw_indexed(0..*count, 0, 0..1);
        } else if lattice.count > lattice.split {
            pass.set_pipeline(&self.rivers);
            pass.draw_indexed(lattice.split..lattice.count, 0, 0..1);
        }
    }
}

fn upload_lattice(d: &wgpu::Device, inland: &Inland, key: usize) -> Option<Lattice> {
    use wgpu::util::DeviceExt;
    let (indices, split) = inland.triangles();
    if indices.is_empty() || inland.looks.is_empty() {
        return None;
    }
    let buffer = |label, contents: &[u8], usage| {
        d.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some(label),
            contents,
            usage,
        })
    };
    let storage = wgpu::BufferUsages::STORAGE;
    let strips = &inland.strips;
    let strip_vertices = if strips.vertices.is_empty() {
        buffer("1800 river strips (none)", &[0; 36], storage)
    } else {
        buffer(
            "1800 river strip vertices",
            bytemuck::cast_slice(&strips.vertices),
            storage,
        )
    };
    let strip_indices = (!strips.indices.is_empty()).then(|| {
        let indices = buffer(
            "1800 river strip indices",
            bytemuck::cast_slice(&strips.indices),
            wgpu::BufferUsages::INDEX,
        );
        (indices, strips.indices.len() as u32)
    });
    Some(Lattice {
        key,
        corners: buffer(
            "1800 inland corners",
            bytemuck::cast_slice(inland.gpu_corners()),
            storage,
        ),
        looks: buffer(
            "1800 inland looks",
            bytemuck::cast_slice(&look_uniforms(inland)),
            storage,
        ),
        indices: buffer(
            "1800 inland indices",
            bytemuck::cast_slice(&indices),
            wgpu::BufferUsages::INDEX,
        ),
        split,
        count: indices.len() as u32,
        strip_vertices,
        strip_indices,
    })
}

/// An RGBA8 mip chain, linear: these hold data, not colours.
pub(super) fn upload(
    d: &wgpu::Device,
    queue: &wgpu::Queue,
    label: &str,
    mips: &[Texture],
) -> wgpu::TextureView {
    let texture = d.create_texture(&wgpu::TextureDescriptor {
        label: Some(label),
        size: wgpu::Extent3d {
            width: mips[0].width,
            height: mips[0].height,
            depth_or_array_layers: 1,
        },
        mip_level_count: mips.len() as u32,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8Unorm,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    for (level, mip) in mips.iter().enumerate() {
        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &texture,
                mip_level: level as u32,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            &mip.rgba,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(mip.width * 4),
                rows_per_image: Some(mip.height),
            },
            wgpu::Extent3d {
                width: mip.width,
                height: mip.height,
                depth_or_array_layers: 1,
            },
        );
    }
    texture.create_view(&Default::default())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn look(fields: Vec<(&str, Vec<f32>)>) -> Look {
        Look {
            lake: false,
            fields: fields.into_iter().map(|(n, v)| (n.into(), v)).collect(),
        }
    }

    // Every River record on moderate_c_01 reads as stored: its rivers'
    // dark blue 0xff031c32, and turbidity in the range shipped rivers use.
    #[test]
    fn shipped_river_records_read() {
        use anno_core::island::objects::Objects;
        use anno_formats::{adf::Adf, rda::Archive};
        let Some(dir) = anno_formats::vfs::install_dir("ANNO1800_DIR") else {
            return;
        };
        let install = Install::open(dir, crate::ARCHIVE_FAMILIES).unwrap();
        let path = "data/dlc01/sessions/islands/pool/moderate_c_01/moderate_c_01.a7m";
        let archive = Archive::parse(install.read(path).unwrap(), path).unwrap();
        let gamedata = Adf::parse(&archive.read_path("gamedata.data").unwrap()).unwrap();
        let store = Objects::read(&gamedata).unwrap();
        // The River asset's GUID in 1800's assets.xml.
        let records: Vec<Record> = (store.objects.iter())
            .filter(|o| o.guid == 1_001_608)
            .map(|o| {
                Record::read(&Look {
                    lake: false,
                    fields: store.component_floats(o, "River"),
                })
            })
            .collect();
        assert!(records.len() > 10, "{} rivers", records.len());
        let blue = [0x03 as f32 / 255., 0x1c as f32 / 255., 0x32 as f32 / 255.];
        for r in &records {
            assert_eq!(r.color, blue, "{r:?}");
            assert!(r.turbidity > 0. && r.turbidity <= 3., "{r:?}");
        }
    }

    // A record's packed ARGB colour and turbidity; what it leaves out takes
    // the serializer's defaults.
    #[test]
    fn records_read_their_colour_and_turbidity() {
        let teal = f32::from_bits(0xff12_9ab2);
        let r = Record::read(&look(vec![
            ("WaterColor", vec![teal]),
            ("Turbidity", vec![1.5]),
        ]));
        assert_eq!(
            r.color,
            [0x12 as f32 / 255., 0x9a as f32 / 255., 0xb2 as f32 / 255.]
        );
        assert_eq!(r.turbidity, 1.5);
        let bare = Record::read(&look(Vec::new()));
        assert_eq!(
            bare.color,
            [0x0a as f32 / 255., 0x55 as f32 / 255., 0x96 as f32 / 255.]
        );
        assert_eq!(bare.turbidity, 1.);
    }
}
