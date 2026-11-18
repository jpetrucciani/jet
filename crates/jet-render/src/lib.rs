//! wgpu renderer for pane snapshots. Draws into any texture view: a window
//! surface, or an offscreen texture via [`Gpu::render_rgba`].
//!
//! Colors are sRGB values blended as-is (no linear-space blending) on a
//! non-sRGB target; gamma and contrast tuning is M5 work.

mod frame;

use jet_core::Snapshot;
use jet_font::{Atlas, CellMetrics, Fonts};
use std::collections::HashMap;
use wgpu::util::DeviceExt;

use crate::frame::GlyphInstance;
pub use crate::frame::{Frame, Quad, Selection};

#[derive(Debug)]
pub enum Error {
    NoAdapter(String),
    Device(String),
    Readback(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoAdapter(err) => write!(f, "no GPU adapter: {err}"),
            Self::Device(err) => write!(f, "GPU device: {err}"),
            Self::Readback(err) => write!(f, "reading back the frame: {err}"),
        }
    }
}

impl std::error::Error for Error {}

/// An adapter, device and queue.
pub struct Gpu {
    pub adapter: wgpu::Adapter,
    pub device: wgpu::Device,
    pub queue: wgpu::Queue,
}

/// Format the renderer draws in. Non-sRGB so colors are written unconverted.
pub const OFFSCREEN_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;

impl Gpu {
    /// Vulkan on Linux, Metal on macOS, DX12 alone on Windows: loading
    /// Vulkan there as well cost ~85 ms of startup for an adapter never
    /// picked. A `see_through` window gets a DirectComposition swapchain,
    /// the only DX12 kind with alpha; an opaque one the plain window
    /// swapchain, which presents on its own monitor's vblank
    /// (DirectComposition's clock skipped every sixth frame on a 120 Hz
    /// monitor beside 144 Hz ones).
    pub fn instance(see_through: bool) -> wgpu::Instance {
        let mut desc = wgpu::InstanceDescriptor::new_without_display_handle();
        desc.backends = if cfg!(windows) {
            wgpu::Backends::DX12
        } else {
            wgpu::Backends::PRIMARY
        };
        desc.backend_options.dx12.presentation_system = if see_through {
            wgpu::Dx12SwapchainKind::DxgiFromVisual
        } else {
            wgpu::Dx12SwapchainKind::DxgiFromHwnd
        };
        wgpu::Instance::new(desc)
    }

    /// `software` picks the fallback adapter (lavapipe, WARP) so output is
    /// the same on every machine; goldens use it.
    pub fn headless(software: bool) -> Result<Self, Error> {
        Self::new(&Self::instance(false), None, software)
    }

    /// A device that can present to `surface`. Without `software`, a real
    /// GPU is preferred and the fallback adapter is used when there is none
    /// (e.g. a VM without GPU passthrough).
    pub fn new(
        instance: &wgpu::Instance,
        surface: Option<&wgpu::Surface<'_>>,
        software: bool,
    ) -> Result<Self, Error> {
        let request = |fallback| {
            pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::HighPerformance,
                force_fallback_adapter: fallback,
                compatible_surface: surface,
                ..Default::default()
            }))
        };
        let adapter = match request(software) {
            Err(_) if !software => request(true),
            found => found,
        }
        .map_err(|err| Error::NoAdapter(err.to_string()))?;
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("jet"),
            required_limits: wgpu::Limits::downlevel_defaults().using_resolution(adapter.limits()),
            // jet's buffers and atlases total a few MB; the default hint
            // reserves 128 MB device and 64 MB upload blocks for them.
            memory_hints: wgpu::MemoryHints::MemoryUsage,
            ..Default::default()
        }))
        .map_err(|err| Error::Device(err.to_string()))?;
        Ok(Self {
            adapter,
            device,
            queue,
        })
    }

    /// Render one snapshot offscreen and return tightly packed RGBA rows.
    pub fn render_rgba(
        &self,
        renderer: &mut Renderer,
        snapshot: &Snapshot,
    ) -> Result<(u32, u32, Vec<u8>), Error> {
        let (width, height) = renderer.pixel_size(snapshot.cols, snapshot.rows);
        let size = wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        };
        let texture = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("offscreen"),
            size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: OFFSCREEN_FORMAT,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        renderer.render(snapshot, &view, width, height);

        let row = width * 4;
        let padded_row =
            row.div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT) * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
        let buffer = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("readback"),
            size: u64::from(padded_row * height),
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
        encoder.copy_texture_to_buffer(
            texture.as_image_copy(),
            wgpu::TexelCopyBufferInfo {
                buffer: &buffer,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(padded_row),
                    rows_per_image: Some(height),
                },
            },
            size,
        );
        self.queue.submit([encoder.finish()]);

        let slice = buffer.slice(..);
        let (tx, rx) = std::sync::mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |res| {
            let _ = tx.send(res);
        });
        self.device
            .poll(wgpu::PollType::wait_indefinitely())
            .map_err(|err| Error::Readback(err.to_string()))?;
        rx.recv()
            .map_err(|err| Error::Readback(err.to_string()))?
            .map_err(|err| Error::Readback(err.to_string()))?;
        let mapped = slice
            .get_mapped_range()
            .map_err(|err| Error::Readback(err.to_string()))?;
        let mut rgba = Vec::with_capacity((row * height) as usize);
        for chunk in mapped.chunks(padded_row as usize) {
            rgba.extend_from_slice(chunk.get(..row as usize).unwrap_or(chunk));
        }
        Ok((width, height, rgba))
    }
}

/// Growable instance buffer.
struct Instances {
    buffer: wgpu::Buffer,
    capacity: u64,
}

impl Instances {
    fn new(device: &wgpu::Device, label: &str) -> Self {
        Self {
            buffer: Self::alloc(device, label, 4096),
            capacity: 4096,
        }
    }

    fn alloc(device: &wgpu::Device, label: &str, size: u64) -> wgpu::Buffer {
        device.create_buffer(&wgpu::BufferDescriptor {
            label: Some(label),
            size,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        })
    }

    fn upload(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, data: &[u8]) {
        let len = data.len() as u64;
        if len > self.capacity {
            self.capacity = len.next_power_of_two();
            self.buffer = Self::alloc(device, "instances", self.capacity);
        }
        if len > 0 {
            queue.write_buffer(&self.buffer, 0, data);
        }
    }
}

pub struct Renderer {
    device: wgpu::Device,
    queue: wgpu::Queue,
    rect_pipeline: wgpu::RenderPipeline,
    glyph_pipeline: wgpu::RenderPipeline,
    color_glyph_pipeline: wgpu::RenderPipeline,
    picture_pipeline: wgpu::RenderPipeline,
    picture_layout: wgpu::BindGroupLayout,
    picture_sampler: wgpu::Sampler,
    /// Each Kitty graphics picture drawn last frame, by its key: a texture
    /// kept alive by its bind group.
    pictures: HashMap<u64, wgpu::BindGroup>,
    picture_instances: Instances,
    picture_scratch: Vec<GlyphInstance>,
    picture_draws: Vec<PictureDraw>,
    bind_group: wgpu::BindGroup,
    globals: wgpu::Buffer,
    atlas: wgpu::Texture,
    color_atlas: wgpu::Texture,
    rects: Instances,
    glyphs: Instances,
    color_glyphs: Instances,
    fonts: Fonts,
    /// One per grid drawn last time, keyed by the caller's id: each keeps
    /// its unchanged rows' instances.
    frames: HashMap<u64, Frame>,
    /// Grid backgrounds and `under` rects, then `over` rects and overlays,
    /// uploaded as one buffer.
    rect_scratch: Vec<Quad>,
    glyph_scratch: Vec<GlyphInstance>,
    color_glyph_scratch: Vec<GlyphInstance>,
    padding: u32,
    /// Whole-window tint drawn last (the visual bell), if any.
    flash: Option<[f32; 4]>,
    /// Of default backgrounds, for a target that composites with alpha.
    opacity: f32,
    selection: Selection,
    /// Where each added image sits in the color atlas, in UV units.
    images: Vec<([f32; 2], [f32; 2])>,
}

impl Renderer {
    pub fn new(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        format: wgpu::TextureFormat,
        fonts: Fonts,
        padding: u32,
    ) -> Self {
        let shader = device.create_shader_module(wgpu::include_wgsl!("shader.wgsl"));
        let texture = |label, size, format| {
            device.create_texture(&wgpu::TextureDescriptor {
                label: Some(label),
                size: wgpu::Extent3d {
                    width: size,
                    height: size,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            })
        };
        let atlas = texture(
            "glyph atlas",
            fonts.atlas().size(),
            wgpu::TextureFormat::R8Unorm,
        );
        let color_atlas = texture(
            "color glyph atlas",
            fonts.color_atlas().size(),
            wgpu::TextureFormat::Rgba8Unorm,
        );
        let globals = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("globals"),
            contents: bytemuck::cast_slice(&[0f32; 4]),
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        });
        // Glyph quads land on whole pixels, so nearest sampling is exact.
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor::default());
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("globals + atlas"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::VERTEX,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                texture_entry(1),
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
                texture_entry(3),
            ],
        });
        let atlas_view = atlas.create_view(&wgpu::TextureViewDescriptor::default());
        let color_view = color_atlas.create_view(&wgpu::TextureViewDescriptor::default());
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("globals + atlas"),
            layout: &layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: globals.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(&atlas_view),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::Sampler(&sampler),
                },
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: wgpu::BindingResource::TextureView(&color_view),
                },
            ],
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("jet"),
            bind_group_layouts: &[Some(&layout)],
            immediate_size: 0,
        });
        let picture_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("picture"),
            entries: &[
                texture_entry(0),
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });
        let picture_pipeline_layout =
            device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("jet pictures"),
                bind_group_layouts: &[Some(&layout), Some(&picture_layout)],
                immediate_size: 0,
            });
        // Pictures are scaled to their cells, so they're filtered.
        let picture_sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        let pipeline = |label,
                        layout: &wgpu::PipelineLayout,
                        vs,
                        fs,
                        blend,
                        stride,
                        attributes: &[wgpu::VertexAttribute]| {
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some(label),
                layout: Some(layout),
                vertex: wgpu::VertexState {
                    module: &shader,
                    entry_point: Some(vs),
                    compilation_options: Default::default(),
                    buffers: &[Some(wgpu::VertexBufferLayout {
                        array_stride: stride,
                        step_mode: wgpu::VertexStepMode::Instance,
                        attributes,
                    })],
                },
                primitive: wgpu::PrimitiveState {
                    topology: wgpu::PrimitiveTopology::TriangleStrip,
                    ..Default::default()
                },
                depth_stencil: None,
                multisample: wgpu::MultisampleState::default(),
                fragment: Some(wgpu::FragmentState {
                    module: &shader,
                    entry_point: Some(fs),
                    compilation_options: Default::default(),
                    targets: &[Some(wgpu::ColorTargetState {
                        format,
                        blend: Some(blend),
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                }),
                multiview_mask: None,
                cache: None,
            })
        };
        let rect_pipeline = pipeline(
            "rects",
            &pipeline_layout,
            "rect_vs",
            "rect_fs",
            wgpu::BlendState::ALPHA_BLENDING,
            size_of::<Quad>() as u64,
            &wgpu::vertex_attr_array![0 => Float32x2, 1 => Float32x2, 2 => Float32x4],
        );
        let glyph_attributes = wgpu::vertex_attr_array![
            0 => Float32x2, 1 => Float32x2, 2 => Float32x2, 3 => Float32x2, 4 => Float32x4
        ];
        let glyph_pipeline = pipeline(
            "glyphs",
            &pipeline_layout,
            "glyph_vs",
            "glyph_fs",
            wgpu::BlendState::ALPHA_BLENDING,
            size_of::<GlyphInstance>() as u64,
            &glyph_attributes,
        );
        let color_glyph_pipeline = pipeline(
            "color glyphs",
            &pipeline_layout,
            "glyph_vs",
            "color_glyph_fs",
            wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING,
            size_of::<GlyphInstance>() as u64,
            &glyph_attributes,
        );
        let picture_pipeline = pipeline(
            "pictures",
            &picture_pipeline_layout,
            "glyph_vs",
            "picture_fs",
            wgpu::BlendState::ALPHA_BLENDING,
            size_of::<GlyphInstance>() as u64,
            &glyph_attributes,
        );
        Self {
            device: device.clone(),
            queue: queue.clone(),
            rect_pipeline,
            glyph_pipeline,
            color_glyph_pipeline,
            picture_pipeline,
            picture_layout,
            picture_sampler,
            pictures: HashMap::new(),
            picture_instances: Instances::new(device, "pictures"),
            picture_scratch: Vec::new(),
            picture_draws: Vec::new(),
            bind_group,
            globals,
            atlas,
            color_atlas,
            rects: Instances::new(device, "rects"),
            glyphs: Instances::new(device, "glyphs"),
            color_glyphs: Instances::new(device, "color glyphs"),
            fonts,
            frames: HashMap::new(),
            glyph_scratch: Vec::new(),
            color_glyph_scratch: Vec::new(),
            rect_scratch: Vec::new(),
            padding,
            flash: None,
            opacity: 1.0,
            selection: Selection::default(),
            images: Vec::new(),
        }
    }

    pub fn cell_metrics(&self) -> CellMetrics {
        self.fonts.metrics()
    }

    /// Pixels between the window edge and the grid.
    pub fn padding(&self) -> u32 {
        self.padding
    }

    /// Pixel size of a `cols × rows` grid, padding included.
    pub fn pixel_size(&self, cols: u16, rows: u16) -> (u32, u32) {
        let m = self.fonts.metrics();
        (
            u32::from(cols) * m.width + 2 * self.padding,
            u32::from(rows) * m.height + 2 * self.padding,
        )
    }

    /// The cell under pixel `(x, y)` of a `grid`, clamped onto it.
    pub fn cell_at(&self, x: i32, y: i32, grid: (u16, u16)) -> (u16, u16) {
        let m = self.fonts.metrics();
        let pick = |px: i32, cell: u32, count: u16| {
            let i = (px - self.padding as i32).max(0) as u32 / cell.max(1);
            u16::try_from(i)
                .unwrap_or(u16::MAX)
                .min(count.saturating_sub(1))
        };
        (pick(x, m.width, grid.0), pick(y, m.height, grid.1))
    }

    /// Grid that fits in `width × height` pixels; at least 1×1.
    pub fn grid_size(&self, width: u32, height: u32) -> (u16, u16) {
        let m = self.fonts.metrics();
        let fit = |px: u32, cell: u32| {
            u16::try_from((px.saturating_sub(2 * self.padding) / cell).max(1)).unwrap_or(u16::MAX)
        };
        (fit(width, m.width), fit(height, m.height))
    }

    /// Tint the whole frame with `color` (alpha included) until cleared.
    /// Keep a premultiplied RGBA bitmap for drawing with [`Image`]s. `None`
    /// once the color atlas is full. Ids last as long as this renderer.
    pub fn add_image(&mut self, width: u32, height: u32, rgba: &[u8]) -> Option<ImageId> {
        let atlas = self.fonts.color_atlas_mut();
        let (x, y) = atlas.insert(width, height, rgba)?;
        let size = atlas.size() as f32;
        self.images.push((
            [x as f32 / size, y as f32 / size],
            [width as f32 / size, height as f32 / size],
        ));
        Some(ImageId(self.images.len() - 1))
    }

    /// How opaque default backgrounds (and `clear`) are drawn; the target
    /// must take premultiplied alpha for anything below 1 to show through.
    pub fn set_opacity(&mut self, opacity: f32) {
        self.opacity = opacity.clamp(0.0, 1.0);
    }

    pub fn set_selection(&mut self, selection: Selection) {
        self.selection = selection;
    }

    pub fn set_flash(&mut self, color: Option<[f32; 4]>) {
        self.flash = color;
    }

    /// One grid filling the target, padded on every side.
    pub fn render(
        &mut self,
        snapshot: &Snapshot,
        target: &wgpu::TextureView,
        width: u32,
        height: u32,
    ) {
        let grid = Grid {
            id: 0,
            snapshot,
            area: [0.0, 0.0, width as f32, height as f32],
            origin: [self.padding, self.padding],
        };
        let clear = rgba(snapshot.colors.background);
        self.render_grids(&[grid], &[], &[], clear, target, width, height);
    }

    /// `grids` over `clear` with `images` on them, then `overlays`
    /// (dividers, dimming) and the flash on top.
    #[allow(clippy::too_many_arguments)]
    pub fn render_grids(
        &mut self,
        grids: &[Grid<'_>],
        images: &[Image],
        overlays: &[Quad],
        clear: [f32; 4],
        target: &wgpu::TextureView,
        width: u32,
        height: u32,
    ) {
        self.frames
            .retain(|id, _| grids.iter().any(|g| g.id == *id));
        for grid in grids {
            let frame = self.frames.entry(grid.id).or_default();
            frame.selection = self.selection;
            frame.build(grid.snapshot, &mut self.fonts, grid.origin);
        }
        upload_atlas(&self.queue, &self.atlas, self.fonts.atlas_mut());
        upload_atlas(&self.queue, &self.color_atlas, self.fonts.color_atlas_mut());
        self.queue.write_buffer(
            &self.globals,
            0,
            bytemuck::cast_slice(&[width as f32, height as f32, 0.0, 0.0]),
        );
        self.collect_pictures(grids, width, height);
        self.rect_scratch.clear();
        self.glyph_scratch.clear();
        self.color_glyph_scratch.clear();
        let frames = || {
            grids
                .iter()
                .filter_map(|g| Some((g, self.frames.get(&g.id)?)))
        };
        for (grid, frame) in frames() {
            let [x, y, w, h] = grid.area;
            let [r, g, b, a] = frame.clear;
            self.rect_scratch.push(Quad {
                pos: [x, y],
                size: [w, h],
                color: [r, g, b, a * self.opacity],
            });
            self.rect_scratch.extend_from_slice(frame.under());
            self.glyph_scratch.extend_from_slice(frame.glyphs());
            self.color_glyph_scratch
                .extend_from_slice(frame.color_glyphs());
        }
        for image in images {
            if let Some(&(uv_pos, uv_size)) = self.images.get(image.id.0) {
                self.color_glyph_scratch.push(GlyphInstance {
                    pos: image.pos,
                    size: image.size,
                    uv_pos,
                    uv_size,
                    color: [1.0; 4],
                });
            }
        }
        let under = self.rect_scratch.len();
        for (_, frame) in frames() {
            self.rect_scratch.extend_from_slice(frame.over());
        }
        self.rect_scratch.extend_from_slice(overlays);
        if let Some(color) = self.flash {
            self.rect_scratch.push(Quad {
                pos: [0.0, 0.0],
                size: [width as f32, height as f32],
                color,
            });
        }
        self.rects.upload(
            &self.device,
            &self.queue,
            bytemuck::cast_slice(&self.rect_scratch),
        );
        self.glyphs.upload(
            &self.device,
            &self.queue,
            bytemuck::cast_slice(&self.glyph_scratch),
        );
        self.color_glyphs.upload(
            &self.device,
            &self.queue,
            bytemuck::cast_slice(&self.color_glyph_scratch),
        );
        self.picture_instances.upload(
            &self.device,
            &self.queue,
            bytemuck::cast_slice(&self.picture_scratch),
        );
        // The target holds premultiplied color; shaders blend straight
        // colors into it, which keeps it premultiplied.
        let a = f64::from(clear[3] * self.opacity);
        let [r, g, b] = [clear[0], clear[1], clear[2]].map(|c| f64::from(c) * a);
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("frame"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: target,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color { r, g, b, a }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                ..Default::default()
            });
            pass.set_bind_group(0, &self.bind_group, &[]);
            let count = |n: usize| u32::try_from(n).unwrap_or(u32::MAX);
            let (under, total) = (count(under), count(self.rect_scratch.len()));
            pass.set_pipeline(&self.rect_pipeline);
            pass.set_vertex_buffer(0, self.rects.buffer.slice(..));
            pass.draw(0..4, 0..under);
            self.draw_pictures(&mut pass, false, width, height);
            pass.set_pipeline(&self.glyph_pipeline);
            pass.set_vertex_buffer(0, self.glyphs.buffer.slice(..));
            pass.draw(0..4, 0..count(self.glyph_scratch.len()));
            pass.set_pipeline(&self.color_glyph_pipeline);
            pass.set_vertex_buffer(0, self.color_glyphs.buffer.slice(..));
            pass.draw(0..4, 0..count(self.color_glyph_scratch.len()));
            self.draw_pictures(&mut pass, true, width, height);
            pass.set_pipeline(&self.rect_pipeline);
            pass.set_vertex_buffer(0, self.rects.buffer.slice(..));
            pass.draw(0..4, under..total);
        }
        self.queue.submit([encoder.finish()]);
    }
}

impl Renderer {
    /// Instances and clip rects for every grid's Kitty graphics, uploading
    /// pictures not seen before and dropping those no longer shown.
    fn collect_pictures(&mut self, grids: &[Grid<'_>], width: u32, height: u32) {
        self.picture_scratch.clear();
        self.picture_draws.clear();
        for grid in grids {
            let Some(clip) = clip_rect(grid.area, width, height) else {
                continue;
            };
            for placement in &grid.snapshot.images {
                let picture = &placement.picture;
                if !self.pictures.contains_key(&picture.key) {
                    let bind_group = self.upload_picture(picture);
                    self.pictures.insert(picture.key, bind_group);
                }
                let (w, h) = (picture.width as f32, picture.height as f32);
                let [sx, sy, sw, sh] = placement.source.map(|v| v as f32);
                self.picture_scratch.push(GlyphInstance {
                    pos: [
                        grid.origin[0] as f32 + placement.x as f32,
                        grid.origin[1] as f32 + placement.y as f32,
                    ],
                    size: [placement.width as f32, placement.height as f32],
                    uv_pos: [sx / w, sy / h],
                    uv_size: [sw / w, sh / h],
                    color: [1.0; 4],
                });
                self.picture_draws.push(PictureDraw {
                    key: picture.key,
                    clip,
                    above_text: placement.z >= 0,
                });
            }
        }
        let draws = &self.picture_draws;
        self.pictures
            .retain(|key, _| draws.iter().any(|d| d.key == *key));
    }

    fn upload_picture(&self, picture: &jet_core::Picture) -> wgpu::BindGroup {
        let size = wgpu::Extent3d {
            width: picture.width.max(1),
            height: picture.height.max(1),
            depth_or_array_layers: 1,
        };
        let texture = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("picture"),
            size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        self.queue.write_texture(
            texture.as_image_copy(),
            &picture.rgba,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(size.width * 4),
                rows_per_image: None,
            },
            size,
        );
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("picture"),
            layout: &self.picture_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&self.picture_sampler),
                },
            ],
        })
    }

    /// The pictures under the text (`above_text` false) or over it, each
    /// clipped to its pane.
    fn draw_pictures(
        &self,
        pass: &mut wgpu::RenderPass<'_>,
        above_text: bool,
        width: u32,
        height: u32,
    ) {
        let mut any = false;
        for (i, draw) in self.picture_draws.iter().enumerate() {
            let Some(bind_group) = self.pictures.get(&draw.key) else {
                continue;
            };
            if draw.above_text != above_text {
                continue;
            }
            if !any {
                pass.set_pipeline(&self.picture_pipeline);
                pass.set_vertex_buffer(0, self.picture_instances.buffer.slice(..));
                any = true;
            }
            let [x, y, w, h] = draw.clip;
            pass.set_scissor_rect(x, y, w, h);
            pass.set_bind_group(1, bind_group, &[]);
            let i = u32::try_from(i).unwrap_or(u32::MAX);
            pass.draw(0..4, i..i + 1);
        }
        if any {
            pass.set_scissor_rect(0, 0, width.max(1), height.max(1));
        }
    }
}

/// One picture to draw: which, where it may show, and on which side of the
/// text.
struct PictureDraw {
    key: u64,
    clip: [u32; 4],
    above_text: bool,
}

/// `area` (`[x, y, width, height]`) as a scissor rect inside a `width` x
/// `height` target; `None` if nothing of it is inside.
fn clip_rect(area: [f32; 4], width: u32, height: u32) -> Option<[u32; 4]> {
    let [x, y, w, h] = area;
    let x0 = (x.max(0.0) as u32).min(width);
    let y0 = (y.max(0.0) as u32).min(height);
    let x1 = ((x + w).max(0.0) as u32).min(width);
    let y1 = ((y + h).max(0.0) as u32).min(height);
    (x1 > x0 && y1 > y0).then_some([x0, y0, x1 - x0, y1 - y0])
}

/// A bitmap added with [`Renderer::add_image`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ImageId(usize);

/// An added bitmap drawn at `pos` (pixels), `size` big.
#[derive(Clone, Copy, Debug)]
pub struct Image {
    pub id: ImageId,
    pub pos: [f32; 2],
    pub size: [f32; 2],
}

/// One terminal grid to draw.
pub struct Grid<'a> {
    /// Stable per pane, so unchanged rows are reused between frames.
    pub id: u64,
    pub snapshot: &'a Snapshot,
    /// `[x, y, width, height]` filled with the grid's background, padding
    /// included.
    pub area: [f32; 4],
    /// Top-left of the first cell.
    pub origin: [u32; 2],
}

/// An opaque color as the shaders take it.
pub fn rgba(c: jet_core::Rgb) -> [f32; 4] {
    [
        f32::from(c.r) / 255.0,
        f32::from(c.g) / 255.0,
        f32::from(c.b) / 255.0,
        1.0,
    ]
}

fn texture_entry(binding: u32) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::FRAGMENT,
        ty: wgpu::BindingType::Texture {
            sample_type: wgpu::TextureSampleType::Float { filterable: true },
            view_dimension: wgpu::TextureViewDimension::D2,
            multisampled: false,
        },
        count: None,
    }
}

/// Copy the rows of `atlas` written since the last upload into `texture`.
fn upload_atlas(queue: &wgpu::Queue, texture: &wgpu::Texture, atlas: &mut Atlas) {
    let Some(rows) = atlas.take_dirty() else {
        return;
    };
    let (size, row_bytes) = (atlas.size(), atlas.size() * atlas.channels());
    queue.write_texture(
        wgpu::TexelCopyTextureInfo {
            origin: wgpu::Origin3d {
                x: 0,
                y: rows.start,
                z: 0,
            },
            ..texture.as_image_copy()
        },
        atlas.pixels(),
        wgpu::TexelCopyBufferLayout {
            offset: u64::from(rows.start) * u64::from(row_bytes),
            bytes_per_row: Some(row_bytes),
            rows_per_image: None,
        },
        wgpu::Extent3d {
            width: size,
            height: rows.end - rows.start,
            depth_or_array_layers: 1,
        },
    );
}
