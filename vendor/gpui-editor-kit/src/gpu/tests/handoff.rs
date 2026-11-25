// Tests of the handshake a viewport's thread and GPUI keep over the texture
// it draws into (gpui's `TextureHandoff`): GPUI waits for the frame it
// asked for, draws the last one when that misses the deadline, and the
// producer's next submit waits for GPUI's read. Plus the composite's GPU
// cost at 5K, as a benchmark.
use super::*;
use gpui::{HandoffGpu, HandoffTexture, TextureHandoff};
use std::time::{Duration, Instant};

fn gpu() -> HandoffGpu {
    let target = Offscreen::new(4, 4).unwrap();
    HandoffGpu {
        device: target.device().clone(),
        queue: target.queue().clone(),
    }
}

fn texture(gpu: &HandoffGpu, size: [u32; 2]) -> HandoffTexture {
    let texture = gpu.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("handoff test"),
        size: wgpu::Extent3d {
            width: size[0],
            height: size[1],
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: VIEW_FORMAT,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
        view_formats: &[],
    });
    HandoffTexture {
        device: gpu.device.clone(),
        view: texture.create_view(&Default::default()),
        size,
    }
}

/// GPUI's side of one window frame: asks, then reads as its renderer
/// would just before drawing, and ends the read as its submit does. The
/// size drawn, and how long the read waited.
fn window_frame(handoff: &TextureHandoff, gpu: &HandoffGpu) -> (Option<[u32; 2]>, Duration) {
    handoff.ask();
    read(handoff, gpu)
}

fn read(handoff: &TextureHandoff, gpu: &HandoffGpu) -> (Option<[u32; 2]>, Duration) {
    let started = Instant::now();
    let drawn = handoff
        .begin_read(gpu, Duration::ZERO)
        .map(|(_, size)| size);
    let waited = started.elapsed();
    handoff.end_read();
    (drawn, waited)
}

/// Whether the last read drew an older frame than it asked for.
fn late(handoff: &TextureHandoff) -> bool {
    (handoff.take_composite()).is_some_and(|c| c.drew < c.asked)
}

/// A producer answering each ask after `record`, its texture `size`ed by
/// the frame's number so a test can tell which frame GPUI drew. Returns
/// when each frame's submit ran.
fn producer(
    handoff: Arc<TextureHandoff>,
    record: Duration,
) -> std::thread::JoinHandle<Vec<(u64, Instant)>> {
    std::thread::spawn(move || {
        let mut submitted = Vec::new();
        while let Some((frame, gpu)) = handoff.wait() {
            std::thread::sleep(record);
            let made = texture(&gpu, [frame as u32, 1]);
            handoff.submit(frame, || {
                submitted.push((frame, Instant::now()));
                Some(made)
            });
        }
        submitted
    })
}

/// The first window frame tells the producer the device, draws nothing
/// and does not wait; then the producer answers that frame's ask.
fn first_frame(handoff: &TextureHandoff, gpu: &HandoffGpu, record: Duration) {
    let (drawn, waited) = window_frame(handoff, gpu);
    assert_eq!(drawn, None);
    assert!(waited < Duration::from_millis(5), "waited {waited:?}");
    std::thread::sleep(record + Duration::from_millis(100));
}

#[test]
fn a_window_frame_draws_the_frame_it_asked_for() {
    let gpu = gpu();
    let record = Duration::from_millis(30);
    let handoff = Arc::new(TextureHandoff::new(Duration::from_secs(5)));
    let thread = producer(handoff.clone(), record);
    first_frame(&handoff, &gpu, record);
    // Each frame waits out the recording and draws its own frame.
    for _ in 0..3 {
        let (drawn, waited) = window_frame(&handoff, &gpu);
        assert_eq!(drawn, Some([handoff.asked() as u32, 1]));
        assert!(waited >= Duration::from_millis(15), "waited {waited:?}");
        assert!(!late(&handoff));
    }
    handoff.close();
    thread.join().unwrap();
}

#[test]
fn a_late_frame_leaves_the_last_one_and_the_next_shows_it() {
    let gpu = gpu();
    let record = Duration::from_millis(150);
    let handoff = Arc::new(TextureHandoff::new(Duration::from_millis(20)));
    let thread = producer(handoff.clone(), record);
    first_frame(&handoff, &gpu, record);
    // Past the 20 ms deadline: the last complete frame, and a late mark.
    let (drawn, waited) = window_frame(&handoff, &gpu);
    assert_eq!(drawn, Some([1, 1]));
    assert!(waited >= Duration::from_millis(15), "waited {waited:?}");
    assert!(waited < Duration::from_millis(100), "waited {waited:?}");
    assert!(late(&handoff));
    // Still recording the frame before: no wait for one that can't come.
    let (drawn, waited) = window_frame(&handoff, &gpu);
    assert_eq!(drawn, Some([1, 1]));
    assert!(waited < Duration::from_millis(5), "waited {waited:?}");
    std::thread::sleep(2 * record + Duration::from_millis(100));
    // Caught up, folding the two asks into one frame: the window's
    // follow-up frame draws it without waiting.
    let (drawn, waited) = read(&handoff, &gpu);
    assert_eq!(drawn, Some([handoff.asked() as u32, 1]));
    assert!(waited < Duration::from_millis(5), "waited {waited:?}");
    handoff.close();
    thread.join().unwrap();
}

#[test]
fn a_producer_submits_only_once_the_frame_reading_its_texture_is_submitted() {
    let gpu = gpu();
    let handoff = Arc::new(TextureHandoff::new(Duration::from_millis(1)));
    let thread = producer(handoff.clone(), Duration::ZERO);
    first_frame(&handoff, &gpu, Duration::ZERO);
    // A window frame between its read and its submit, and an ask the
    // producer answers at once meanwhile.
    let reading = handoff.begin_read(&gpu, Duration::ZERO).map(|(_, s)| s);
    handoff.ask();
    std::thread::sleep(Duration::from_millis(60));
    let ended = Instant::now();
    handoff.end_read();
    std::thread::sleep(Duration::from_millis(60));
    let (after, _) = read(&handoff, &gpu);
    handoff.close();
    let submitted = thread.join().unwrap();
    assert_eq!(reading, Some([1, 1]));
    assert_eq!(after, Some([2, 1]));
    let (_, second) = submitted.iter().find(|(f, _)| *f == 2).expect("frame 2");
    assert!(*second >= ended, "frame 2 was submitted during the read");
}

/// The GPU time of GPUI drawing a viewport's texture into its window: the
/// polychrome sprite pipeline from GPUI's own shader source, a 1830x1312
/// pane at scale 2 (gpu_bench's) into a 5120x2880 target. Each draw gets a
/// pass of its own, after a clear of another 5K target that pushes the
/// texture out of the GPU's caches, as a frame's 3D work would.
///
/// `cargo test --release -p gpui-editor-kit composite_cost -- --ignored --nocapture`
#[test]
#[ignore = "benchmark"]
fn composite_cost_at_5k() {
    let target = Offscreen::new(4, 4).unwrap();
    let (device, queue) = (target.device().clone(), target.queue().clone());
    assert!(
        device.features().contains(wgpu::Features::TIMESTAMP_QUERY),
        "needs timestamp queries"
    );
    let format = wgpu::TextureFormat::Bgra8Unorm;
    let (window, pane) = ([5120u32, 2880], [3660u32, 2624]);
    let source = concat!(
        include_str!("../../../vendor/zed/gpui_wgpu/src/shaders.wgsl"),
        include_str!("../../../vendor/zed/gpui_wgpu/src/shaders_storage.wgsl"),
    );
    let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("gpui shaders"),
        source: wgpu::ShaderSource::Wgsl(source.into()),
    });
    // GPUI's layouts: globals, instances, and the sprite's texture, which
    // the vertex stage sizes and the fragment stage samples.
    let both = wgpu::ShaderStages::VERTEX_FRAGMENT;
    let layout = |entries: &[wgpu::BindGroupLayoutEntry]| {
        device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: None,
            entries,
        })
    };
    let slot = |binding, ty| wgpu::BindGroupLayoutEntry {
        binding,
        visibility: both,
        ty,
        count: None,
    };
    let layouts = [
        layout(&[slot(
            0,
            wgpu::BindingType::Buffer {
                ty: wgpu::BufferBindingType::Uniform,
                has_dynamic_offset: false,
                min_binding_size: None,
            },
        )]),
        layout(&[slot(
            0,
            wgpu::BindingType::Buffer {
                ty: wgpu::BufferBindingType::Storage { read_only: true },
                has_dynamic_offset: false,
                min_binding_size: None,
            },
        )]),
        layout(&[
            slot(
                0,
                wgpu::BindingType::Texture {
                    sample_type: wgpu::TextureSampleType::Float { filterable: true },
                    view_dimension: wgpu::TextureViewDimension::D2,
                    multisampled: false,
                },
            ),
            slot(
                1,
                wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
            ),
        ]),
    ];
    let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: None,
        bind_group_layouts: &[Some(&layouts[0]), Some(&layouts[1]), Some(&layouts[2])],
        immediate_size: 0,
    });
    let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("gpui poly sprites"),
        layout: Some(&pipeline_layout),
        vertex: wgpu::VertexState {
            module: &module,
            entry_point: Some("vs_poly_sprite"),
            buffers: &[],
            compilation_options: Default::default(),
        },
        fragment: Some(wgpu::FragmentState {
            module: &module,
            entry_point: Some("fs_poly_sprite"),
            targets: &[Some(wgpu::ColorTargetState {
                format,
                blend: Some(wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING),
                write_mask: wgpu::ColorWrites::ALL,
            })],
            compilation_options: Default::default(),
        }),
        primitive: wgpu::PrimitiveState {
            topology: wgpu::PrimitiveTopology::TriangleStrip,
            ..Default::default()
        },
        depth_stencil: None,
        multisample: Default::default(),
        multiview_mask: None,
        cache: None,
    });
    let make = |size: [u32; 2], usage| {
        device.create_texture(&wgpu::TextureDescriptor {
            label: None,
            size: wgpu::Extent3d {
                width: size[0],
                height: size[1],
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage,
            view_formats: &[],
        })
    };
    let attach = wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_DST;
    // Noise in both, as a frame's pixels are: blank textures compress, and
    // were read and written at several times the memory's bandwidth.
    let noisy = |size: [u32; 2], usage| {
        let texture = make(size, usage);
        let mut seed = 0x9e37_79b9_u32;
        let bytes: Vec<u8> = (0..size[0] * size[1])
            .flat_map(|_| {
                seed ^= seed << 13;
                seed ^= seed >> 17;
                seed ^= seed << 5;
                (seed | 0xff00_0000).to_le_bytes()
            })
            .collect();
        queue.write_texture(
            texture.as_image_copy(),
            &bytes,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(size[0] * 4),
                rows_per_image: None,
            },
            texture.size(),
        );
        texture.create_view(&Default::default())
    };
    let frame = noisy(window, attach);
    let evict = make(window, attach).create_view(&Default::default());
    let pane_view = noisy(pane, attach | wgpu::TextureUsages::TEXTURE_BINDING);
    // GlobalParams: viewport size, premultiplied alpha, pad, path size.
    let mut globals = Vec::new();
    for v in [window[0] as f32, window[1] as f32] {
        globals.extend(v.to_le_bytes());
    }
    globals.extend([1u32, 0].iter().flat_map(|v| v.to_le_bytes()));
    globals.extend([0f32; 4].iter().flat_map(|v| v.to_le_bytes()));
    // One PolychromeSprite: order, pad, tile (texture id, kind, tile id,
    // padding, bounds), grayscale, opacity, bounds, content mask, radii.
    let at = [700f32, 128.];
    let size = [pane[0] as f32, pane[1] as f32];
    let mut sprite: Vec<u8> = Vec::new();
    for v in [0u32, 0, 0, 1, 0, 0, 0, 0, pane[0], pane[1], 0] {
        sprite.extend(v.to_le_bytes());
    }
    let floats = [
        1., at[0], at[1], size[0], size[1], at[0], at[1], size[0], size[1],
    ];
    for v in floats.into_iter().chain([0f32; 4]) {
        sprite.extend(v.to_le_bytes());
    }
    let buffer = |bytes: &[u8], usage| {
        wgpu::util::DeviceExt::create_buffer_init(
            &device,
            &wgpu::util::BufferInitDescriptor {
                label: None,
                contents: bytes,
                usage,
            },
        )
    };
    let globals = buffer(&globals, wgpu::BufferUsages::UNIFORM);
    let sprites = buffer(&sprite, wgpu::BufferUsages::STORAGE);
    let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
        mag_filter: wgpu::FilterMode::Linear,
        min_filter: wgpu::FilterMode::Linear,
        ..Default::default()
    });
    let group = |i: usize, entries: &[wgpu::BindGroupEntry]| {
        device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: None,
            layout: &layouts[i],
            entries,
        })
    };
    let entry = |binding, resource| wgpu::BindGroupEntry { binding, resource };
    let groups = [
        group(0, &[entry(0, globals.as_entire_binding())]),
        group(1, &[entry(0, sprites.as_entire_binding())]),
        group(
            2,
            &[
                entry(0, wgpu::BindingResource::TextureView(&pane_view)),
                entry(1, wgpu::BindingResource::Sampler(&sampler)),
            ],
        ),
    ];
    const RUNS: u32 = 64;
    let queries = device.create_query_set(&wgpu::QuerySetDescriptor {
        label: None,
        ty: wgpu::QueryType::Timestamp,
        count: RUNS * 2,
    });
    let resolve = device.create_buffer(&wgpu::BufferDescriptor {
        label: None,
        size: u64::from(RUNS) * 16,
        usage: wgpu::BufferUsages::QUERY_RESOLVE | wgpu::BufferUsages::COPY_SRC,
        mapped_at_creation: false,
    });
    let read = device.create_buffer(&wgpu::BufferDescriptor {
        label: None,
        size: u64::from(RUNS) * 16,
        usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    let mut encoder = device.create_command_encoder(&Default::default());
    for run in 0..RUNS {
        drop(encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("evict"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &evict,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                    store: wgpu::StoreOp::Store,
                },
                depth_slice: None,
            })],
            ..Default::default()
        }));
        let times = wgpu::RenderPassTimestampWrites {
            query_set: &queries,
            beginning_of_pass_write_index: Some(run * 2),
            end_of_pass_write_index: Some(run * 2 + 1),
        };
        let mut composite = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("composite"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &frame,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Load,
                    store: wgpu::StoreOp::Store,
                },
                depth_slice: None,
            })],
            timestamp_writes: Some(times),
            ..Default::default()
        });
        composite.set_pipeline(&pipeline);
        for (i, g) in groups.iter().enumerate() {
            composite.set_bind_group(i as u32, g, &[]);
        }
        composite.draw(0..4, 0..1);
    }
    encoder.resolve_query_set(&queries, 0..RUNS * 2, &resolve, 0);
    encoder.copy_buffer_to_buffer(&resolve, 0, &read, 0, u64::from(RUNS) * 16);
    queue.submit([encoder.finish()]);
    read.slice(..).map_async(wgpu::MapMode::Read, |_| {});
    device.poll(wgpu::PollType::wait_indefinitely()).unwrap();
    let mapped = read.slice(..).get_mapped_range().unwrap();
    let stamps: Vec<u64> = bytemuck::cast_slice(&mapped).to_vec();
    let period = f64::from(queue.get_timestamp_period());
    let mut ms: Vec<f64> = (stamps.chunks(2))
        .map(|t| t[1].saturating_sub(t[0]) as f64 * period / 1e6)
        .collect();
    ms.sort_by(f64::total_cmp);
    eprintln!(
        "composite of a {}x{} pane into a {}x{} target: median {:.3} ms, min {:.3}, max {:.3} over {RUNS}",
        pane[0],
        pane[1],
        window[0],
        window[1],
        ms[ms.len() / 2],
        ms[0],
        ms[ms.len() - 1]
    );
}
