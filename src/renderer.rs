//! GPU renderer: glyph atlases plus instanced quads, the same basic design as
//! Alacritty/Ghostty/Kitty. Each frame is a single draw call: one instance per
//! background rect, glyph, emoji, or the background image.

use std::collections::HashMap;
use std::sync::Arc;

use bytemuck::{Pod, Zeroable};
use winit::window::Window;

use crate::config::Config;
use crate::text::{self, Shaped, Text};

// 1024² R8 = 1 MB of GPU memory, ~1500 glyphs at Retina sizes; reset if it fills up.
const MASK_ATLAS: u32 = 1024;
// 512² RGBA = 1 MB, for color emoji only.
const COLOR_ATLAS: u32 = 512;

const KIND_RECT: u32 = 0;
const KIND_MASK: u32 = 1;
const KIND_COLOR: u32 = 2;
const KIND_IMAGE: u32 = 3;

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
pub struct Instance {
    pos: [f32; 2],
    size: [f32; 2],
    uv_pos: [f32; 2],
    uv_size: [f32; 2],
    color: [f32; 4],
    kind: u32,
    _pad: [u32; 3],
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct Globals {
    screen: [f32; 2],
    _pad: [f32; 2],
}

#[derive(Clone, Copy)]
struct GlyphEntry {
    x: u32,
    y: u32,
    w: u32,
    h: u32,
    left: f32,
    top: f32,
    color: bool,
}

/// Simple shelf packer for a square atlas.
struct Packer {
    size: u32,
    x: u32,
    y: u32,
    row_h: u32,
}

impl Packer {
    fn new(size: u32) -> Self {
        Self { size, x: 0, y: 0, row_h: 0 }
    }

    /// Returns a slot, or None when the atlas is full.
    fn alloc(&mut self, w: u32, h: u32) -> Option<(u32, u32)> {
        if w + 1 > self.size || h + 1 > self.size {
            return None;
        }
        if self.x + w + 1 > self.size {
            self.x = 0;
            self.y += self.row_h + 1;
            self.row_h = 0;
        }
        if self.y + h + 1 > self.size {
            return None;
        }
        let slot = (self.x, self.y);
        self.x += w + 1;
        self.row_h = self.row_h.max(h);
        Some(slot)
    }
}

fn texture(device: &wgpu::Device, label: &str, w: u32, h: u32, format: wgpu::TextureFormat) -> wgpu::Texture {
    device.create_texture(&wgpu::TextureDescriptor {
        label: Some(label),
        size: wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    })
}

pub struct Renderer {
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    surface_cfg: wgpu::SurfaceConfiguration,
    pipeline: wgpu::RenderPipeline,
    bind_layout: wgpu::BindGroupLayout,
    bind_group: wgpu::BindGroup,
    globals_buf: wgpu::Buffer,
    mask_tex: wgpu::Texture,
    color_tex: wgpu::Texture,
    image_tex: wgpu::Texture,
    nearest: wgpu::Sampler,
    linear: wgpu::Sampler,
    instance_buf: wgpu::Buffer,
    instance_cap: usize,
    instances: Vec<Instance>,

    text: Text,
    glyphs: HashMap<(u16, u16), GlyphEntry>,
    mask_packer: Packer,
    color_packer: Packer,
    /// Bumped whenever an atlas is reset, invalidating cached instances.
    atlas_gen: u64,
    pub image_size: Option<(u32, u32)>,
    pub scale: f32,
    pub transparent: bool,
    premultiplied: bool,
}

impl Renderer {
    pub fn new(window: Arc<Window>, display: Box<dyn wgpu::wgt::WgpuHasDisplayHandle>, cfg: &Config) -> Renderer {
        let mut desc = wgpu::InstanceDescriptor::new_with_display_handle_from_env(display);
        // On Windows, composition-based swapchains are what make per-pixel alpha possible (the
        // same approach as Windows Terminal's AtlasEngine). Only DX12 offers that: Vulkan
        // swapchains on Windows are opaque, and wgpu may pick Vulkan first, so prefer DX12 unless
        // WGPU_BACKEND says otherwise.
        desc.backend_options.dx12.presentation_system = wgpu::wgt::Dx12SwapchainKind::DxgiFromVisual;
        let instance = wgpu::Instance::new(desc);
        let surface = instance.create_surface(window.clone()).expect("create surface");
        let dx12 = if cfg!(windows) && std::env::var_os("WGPU_BACKEND").is_none() {
            let adapters = pollster::block_on(instance.enumerate_adapters(wgpu::Backends::DX12));
            let supported = adapters.into_iter().filter(|a| a.is_surface_supported(&surface));
            // Same preference as below: integrated over discrete when both can draw.
            supported.min_by_key(|a| a.get_info().device_type != wgpu::DeviceType::IntegratedGpu)
        } else {
            None
        };
        let adapter = dx12.unwrap_or_else(|| {
            pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::LowPower,
                compatible_surface: Some(&surface),
                force_fallback_adapter: false,
                apply_limit_buckets: false,
            }))
            .expect("no GPU adapter")
        });
        log::info!("GPU adapter: {:?}", adapter.get_info());
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: None,
            required_features: wgpu::Features::empty(),
            required_limits: wgpu::Limits::downlevel_defaults().using_resolution(adapter.limits()),
            memory_hints: wgpu::MemoryHints::MemoryUsage,
            ..Default::default()
        }))
        .expect("request device");

        let caps = surface.get_capabilities(&adapter);
        // Non-sRGB format: terminal colors are specified in sRGB and blended as-is, like other terminals.
        let format = caps.formats.iter().copied().find(|f| !f.is_srgb()).unwrap_or(caps.formats[0]);
        let alpha_mode = [wgpu::CompositeAlphaMode::PreMultiplied, wgpu::CompositeAlphaMode::PostMultiplied, wgpu::CompositeAlphaMode::Inherit]
            .into_iter()
            .find(|m| caps.alpha_modes.contains(m))
            .unwrap_or(caps.alpha_modes[0]);
        let transparent = alpha_mode != wgpu::CompositeAlphaMode::Opaque;
        log::info!("surface format {format:?}, alpha mode {alpha_mode:?} (supported: {:?})", caps.alpha_modes);

        let present_mode = match cfg.window.present_mode.as_str() {
            "fifo" => wgpu::PresentMode::Fifo,
            "immediate" if caps.present_modes.contains(&wgpu::PresentMode::Immediate) => wgpu::PresentMode::Immediate,
            _ if caps.present_modes.contains(&wgpu::PresentMode::Mailbox) => wgpu::PresentMode::Mailbox,
            _ => wgpu::PresentMode::Fifo,
        };

        let size = window.inner_size();
        let surface_cfg = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width: size.width.max(1),
            height: size.height.max(1),
            present_mode,
            desired_maximum_frame_latency: 1,
            alpha_mode,
            view_formats: vec![],
            color_space: wgpu::SurfaceColorSpace::Auto,
        };
        surface.configure(&device, &surface_cfg);

        let shader = device.create_shader_module(wgpu::include_wgsl!("shader.wgsl"));
        let globals_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("globals"),
            size: std::mem::size_of::<Globals>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let mask_tex = texture(&device, "mask atlas", MASK_ATLAS, MASK_ATLAS, wgpu::TextureFormat::R8Unorm);
        let color_tex = texture(&device, "color atlas", COLOR_ATLAS, COLOR_ATLAS, wgpu::TextureFormat::Rgba8Unorm);
        // 1×1 transparent placeholder until a background image is configured.
        let image_tex = texture(&device, "background image", 1, 1, wgpu::TextureFormat::Rgba8Unorm);
        let nearest = device.create_sampler(&wgpu::SamplerDescriptor {
            mag_filter: wgpu::FilterMode::Nearest,
            min_filter: wgpu::FilterMode::Nearest,
            ..Default::default()
        });
        let linear = device.create_sampler(&wgpu::SamplerDescriptor {
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });

        let tex_entry = |binding| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Texture {
                sample_type: wgpu::TextureSampleType::Float { filterable: true },
                view_dimension: wgpu::TextureViewDimension::D2,
                multisampled: false,
            },
            count: None,
        };
        let sampler_entry = |binding| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
            count: None,
        };
        let bind_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: None,
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                    ty: wgpu::BindingType::Buffer { ty: wgpu::BufferBindingType::Uniform, has_dynamic_offset: false, min_binding_size: None },
                    count: None,
                },
                tex_entry(1),
                sampler_entry(2),
                tex_entry(3),
                tex_entry(4),
                sampler_entry(5),
            ],
        });
        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: None,
            bind_group_layouts: &[Some(&bind_layout)],
            immediate_size: 0,
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("quads"),
            layout: Some(&layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                compilation_options: Default::default(),
                buffers: &[Some(wgpu::VertexBufferLayout {
                    array_stride: std::mem::size_of::<Instance>() as u64,
                    step_mode: wgpu::VertexStepMode::Instance,
                    attributes: &wgpu::vertex_attr_array![
                        0 => Float32x2, 1 => Float32x2, 2 => Float32x2, 3 => Float32x2, 4 => Float32x4, 5 => Uint32
                    ],
                })],
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: Some(wgpu::BlendState {
                        color: wgpu::BlendComponent {
                            src_factor: wgpu::BlendFactor::SrcAlpha,
                            dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha,
                            operation: wgpu::BlendOperation::Add,
                        },
                        alpha: wgpu::BlendComponent::OVER,
                    }),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState { topology: wgpu::PrimitiveTopology::TriangleStrip, ..Default::default() },
            depth_stencil: None,
            multisample: Default::default(),
            multiview_mask: None,
            cache: None,
        });

        let instance_cap = 4096;
        let instance_buf = Self::make_instance_buf(&device, instance_cap);
        let bind_group = Self::make_bind_group(&device, &bind_layout, &globals_buf, &mask_tex, &color_tex, &image_tex, &nearest, &linear);

        let scale = window.scale_factor() as f32;
        let text = Text::load(cfg, scale);

        Renderer {
            surface,
            device,
            queue,
            surface_cfg,
            pipeline,
            bind_layout,
            bind_group,
            globals_buf,
            mask_tex,
            color_tex,
            image_tex,
            nearest,
            linear,
            instance_buf,
            instance_cap,
            instances: Vec::with_capacity(instance_cap),
            text,
            glyphs: HashMap::new(),
            mask_packer: Packer::new(MASK_ATLAS),
            color_packer: Packer::new(COLOR_ATLAS),
            atlas_gen: 0,
            image_size: None,
            scale,
            transparent,
            premultiplied: alpha_mode == wgpu::CompositeAlphaMode::PreMultiplied,
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn make_bind_group(
        device: &wgpu::Device,
        layout: &wgpu::BindGroupLayout,
        globals: &wgpu::Buffer,
        mask: &wgpu::Texture,
        color: &wgpu::Texture,
        image: &wgpu::Texture,
        nearest: &wgpu::Sampler,
        linear: &wgpu::Sampler,
    ) -> wgpu::BindGroup {
        let (mask_v, color_v, image_v) = (mask.create_view(&Default::default()), color.create_view(&Default::default()), image.create_view(&Default::default()));
        device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: None,
            layout,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: globals.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(&mask_v) },
                wgpu::BindGroupEntry { binding: 2, resource: wgpu::BindingResource::Sampler(nearest) },
                wgpu::BindGroupEntry { binding: 3, resource: wgpu::BindingResource::TextureView(&color_v) },
                wgpu::BindGroupEntry { binding: 4, resource: wgpu::BindingResource::TextureView(&image_v) },
                wgpu::BindGroupEntry { binding: 5, resource: wgpu::BindingResource::Sampler(linear) },
            ],
        })
    }

    fn make_instance_buf(device: &wgpu::Device, cap: usize) -> wgpu::Buffer {
        device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("instances"),
            size: (cap * std::mem::size_of::<Instance>()) as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        })
    }

    pub fn resize(&mut self, width: u32, height: u32) {
        self.surface_cfg.width = width.max(1);
        self.surface_cfg.height = height.max(1);
        self.surface.configure(&self.device, &self.surface_cfg);
    }

    pub fn size(&self) -> (f32, f32) {
        (self.surface_cfg.width as f32, self.surface_cfg.height as f32)
    }

    /// Reload fonts (config change or DPI change). Clears the glyph caches.
    pub fn reload_fonts(&mut self, cfg: &Config, scale: f32) {
        self.scale = scale;
        self.text = Text::load(cfg, scale);
        self.reset_atlases();
    }

    fn reset_atlases(&mut self) {
        self.glyphs.clear();
        self.mask_packer = Packer::new(MASK_ATLAS);
        self.color_packer = Packer::new(COLOR_ATLAS);
        self.atlas_gen += 1;
    }

    pub fn atlas_gen(&self) -> u64 {
        self.atlas_gen
    }

    /// Replace the background image (already decoded and sized for the window), or remove it.
    /// The CPU copy is dropped right after upload; only the GPU texture remains.
    pub fn set_background_image(&mut self, image: Option<(Vec<u8>, u32, u32)>) {
        let (tex, size) = match image {
            Some((rgba, w, h)) => {
                let tex = texture(&self.device, "background image", w, h, wgpu::TextureFormat::Rgba8Unorm);
                self.queue.write_texture(
                    wgpu::TexelCopyTextureInfo { texture: &tex, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
                    &rgba,
                    wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(w * 4), rows_per_image: Some(h) },
                    wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
                );
                (tex, Some((w, h)))
            }
            None => (texture(&self.device, "background image", 1, 1, wgpu::TextureFormat::Rgba8Unorm), None),
        };
        self.image_tex = tex;
        self.image_size = size;
        self.bind_group = Self::make_bind_group(
            &self.device,
            &self.bind_layout,
            &self.globals_buf,
            &self.mask_tex,
            &self.color_tex,
            &self.image_tex,
            &self.nearest,
            &self.linear,
        );
    }

    pub fn monospace_families(&self) -> &[String] {
        &self.text.monospace_families
    }

    pub fn has_bosancica(&self) -> bool {
        self.text.has_bosancica
    }

    /// Cell size in physical pixels.
    pub fn cell(&self) -> (f32, f32) {
        (self.text.cell_w, self.text.cell_h)
    }

    pub fn begin(&mut self) {
        self.instances.clear();
    }

    /// Current position in the instance list (for capturing a row into a cache).
    pub fn mark(&self) -> usize {
        self.instances.len()
    }

    pub fn since(&self, mark: usize) -> &[Instance] {
        &self.instances[mark..]
    }

    /// Replay previously captured instances.
    pub fn extend(&mut self, cached: &[Instance]) {
        self.instances.extend_from_slice(cached);
    }

    pub fn rect(&mut self, x: f32, y: f32, w: f32, h: f32, color: [f32; 4]) {
        self.instances.push(Instance { pos: [x, y], size: [w, h], uv_pos: [0.0; 2], uv_size: [0.0; 2], color, kind: KIND_RECT, _pad: [0; 3] });
    }

    /// Background image quad; `uv` is the normalized source rect.
    pub fn image(&mut self, x: f32, y: f32, w: f32, h: f32, uv: [f32; 4], alpha: f32) {
        self.instances.push(Instance {
            pos: [x, y],
            size: [w, h],
            uv_pos: [uv[0], uv[1]],
            uv_size: [uv[2], uv[3]],
            color: [1.0, 1.0, 1.0, alpha],
            kind: KIND_IMAGE,
            _pad: [0; 3],
        });
    }

    /// Draw a run of same-styled cells starting at pixel (x0, y). `cells` holds each
    /// character with its column relative to x0. Ligatures are applied within the run.
    pub fn run(&mut self, cells: &[(char, u16)], style: u8, x0: f32, y: f32, color: [f32; 4]) {
        let (cw, ch) = (self.text.cell_w, self.text.cell_h);
        // Box drawing / block elements are drawn as rects so they join seamlessly across cells;
        // they become spaces for the shaper so the remaining columns stay aligned.
        let mut shaped_cells: Vec<(char, u16)> = Vec::with_capacity(cells.len());
        for &(c, col) in cells {
            let x = x0 + col as f32 * cw;
            if let Some(lines) = text::box_lines(c) {
                self.box_drawing(lines, x, y, color);
                shaped_cells.push((' ', col));
            } else if let Some((r, a)) = text::block(c) {
                self.rect(x + r[0] * cw, y + r[1] * ch, (r[2] - r[0]) * cw, (r[3] - r[1]) * ch, [color[0], color[1], color[2], color[3] * a]);
                shaped_cells.push((' ', col));
            } else {
                shaped_cells.push((c, col));
            }
        }
        if shaped_cells.iter().all(|(c, _)| *c == ' ') {
            return;
        }
        let shaped = self.text.shape(&shaped_cells, style);
        for &Shaped { face, glyph, col, x, y: gy } in shaped.iter() {
            let Some(g) = self.glyph_entry(face, glyph) else { continue };
            if g.w == 0 {
                continue;
            }
            let cell_x = x0 + col as f32 * cw;
            let instance = if g.color {
                // Emoji bitmaps come in fixed strike sizes; fit them into two cells × one line.
                let s = (ch / g.h as f32).min(2.0 * cw / g.w as f32).min(1.0);
                let (w, h) = (g.w as f32 * s, g.h as f32 * s);
                let size = COLOR_ATLAS as f32;
                Instance {
                    pos: [(cell_x + (2.0 * cw - w) / 2.0).round(), (y + (ch - h) / 2.0).round()],
                    size: [w, h],
                    uv_pos: [g.x as f32 / size, g.y as f32 / size],
                    uv_size: [g.w as f32 / size, g.h as f32 / size],
                    color: [1.0, 1.0, 1.0, color[3]],
                    kind: KIND_COLOR,
                    _pad: [0; 3],
                }
            } else {
                let size = MASK_ATLAS as f32;
                Instance {
                    pos: [(cell_x + x + g.left).round(), (y + self.text.baseline - gy - g.top).round()],
                    size: [g.w as f32, g.h as f32],
                    uv_pos: [g.x as f32 / size, g.y as f32 / size],
                    uv_size: [g.w as f32 / size, g.h as f32 / size],
                    color,
                    kind: KIND_MASK,
                    _pad: [0; 3],
                }
            };
            self.instances.push(instance);
        }
    }

    fn box_drawing(&mut self, [up, right, down, left]: [u8; 4], x: f32, y: f32, color: [f32; 4]) {
        let (cw, ch) = (self.text.cell_w, self.text.cell_h);
        let light = (self.scale).round().max(1.0);
        let thick = |w: u8| if w == 2 { light * 2.0 } else { light };
        let (cx, cy) = ((x + cw / 2.0).floor(), (y + ch / 2.0).floor());
        let hw = thick(left.max(right));
        let vw = thick(up.max(down));
        if left > 0 { self.rect(x, cy - (thick(left) / 2.0).floor(), cx - x + vw / 2.0, thick(left), color); }
        if right > 0 { self.rect(cx - (vw / 2.0).floor(), cy - (thick(right) / 2.0).floor(), x + cw - cx + (vw / 2.0).floor(), thick(right), color); }
        if up > 0 { self.rect(cx - (thick(up) / 2.0).floor(), y, thick(up), cy - y + hw / 2.0, color); }
        if down > 0 { self.rect(cx - (thick(down) / 2.0).floor(), cy - (hw / 2.0).floor(), thick(down), y + ch - cy + (hw / 2.0).floor(), color); }
    }

    /// X position right after `s` drawn at `x` (clipped to max_x).
    pub fn text_end(&self, s: &str, x: f32, max_x: f32) -> f32 {
        (x + s.chars().count() as f32 * self.text.cell_w).min(max_x)
    }

    /// Draw plain UI text (tab titles, overlays) clipped to max_x.
    pub fn text(&mut self, s: &str, x: f32, y: f32, max_x: f32, color: [f32; 4]) {
        let fit = ((max_x - x) / self.text.cell_w).floor().max(0.0) as usize;
        let cells: Vec<(char, u16)> = s.chars().take(fit).enumerate().map(|(i, c)| (c, i as u16)).collect();
        self.run(&cells, text::REGULAR, x, y, color);
    }

    fn glyph_entry(&mut self, face: u16, glyph: u16) -> Option<GlyphEntry> {
        if let Some(e) = self.glyphs.get(&(face, glyph)) {
            return Some(*e);
        }
        let empty = GlyphEntry { x: 0, y: 0, w: 0, h: 0, left: 0.0, top: 0.0, color: false };
        let entry = match self.text.rasterize(face, glyph) {
            Some(r) if r.width > 0 && r.height > 0 => {
                let (w, h) = (r.width, r.height);
                let packer = if r.color { &mut self.color_packer } else { &mut self.mask_packer };
                let slot = match packer.alloc(w, h) {
                    Some(s) => s,
                    None => {
                        // Atlas full: start over; glyphs are re-rasterized lazily.
                        self.reset_atlases();
                        let packer = if r.color { &mut self.color_packer } else { &mut self.mask_packer };
                        packer.alloc(w, h)?
                    }
                };
                let (tex, bpp) = if r.color { (&self.color_tex, 4) } else { (&self.mask_tex, 1) };
                self.queue.write_texture(
                    wgpu::TexelCopyTextureInfo { texture: tex, mip_level: 0, origin: wgpu::Origin3d { x: slot.0, y: slot.1, z: 0 }, aspect: wgpu::TextureAspect::All },
                    &r.data,
                    wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(w * bpp), rows_per_image: Some(h) },
                    wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
                );
                GlyphEntry { x: slot.0, y: slot.1, w, h, left: r.left as f32, top: r.top as f32, color: r.color }
            }
            _ => empty,
        };
        self.glyphs.insert((face, glyph), entry);
        Some(entry)
    }

    /// Submit the frame. `clear` is the (non-premultiplied) window background color incl. alpha.
    /// Returns false if no frame reached the screen (the caller must draw again).
    pub fn present(&mut self, clear: [f32; 4]) -> bool {
        let frame = match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(f) | wgpu::CurrentSurfaceTexture::Suboptimal(f) => f,
            wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Lost => {
                self.surface.configure(&self.device, &self.surface_cfg);
                return false;
            }
            _ => return false,
        };

        if self.instances.len() > self.instance_cap {
            self.instance_cap = self.instances.len().next_power_of_two();
            self.instance_buf = Self::make_instance_buf(&self.device, self.instance_cap);
        }
        self.queue.write_buffer(&self.instance_buf, 0, bytemuck::cast_slice(&self.instances));
        let globals = Globals { screen: [self.surface_cfg.width as f32, self.surface_cfg.height as f32], _pad: [0.0; 2] };
        self.queue.write_buffer(&self.globals_buf, 0, bytemuck::bytes_of(&globals));

        let view = frame.texture.create_view(&Default::default());
        let a = if self.transparent { clear[3] } else { 1.0 };
        // PreMultiplied surfaces (e.g. DX12/Vulkan) need premultiplied clear; PostMultiplied (Metal) needs straight.
        let k = if self.premultiplied { a } else { 1.0 };
        let clear_color = wgpu::Color { r: (clear[0] * k) as f64, g: (clear[1] * k) as f64, b: (clear[2] * k) as f64, a: a as f64 };

        let mut encoder = self.device.create_command_encoder(&Default::default());
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: None,
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations { load: wgpu::LoadOp::Clear(clear_color), store: wgpu::StoreOp::Store },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            pass.set_pipeline(&self.pipeline);
            pass.set_bind_group(0, &self.bind_group, &[]);
            pass.set_vertex_buffer(0, self.instance_buf.slice(..));
            pass.draw(0..4, 0..self.instances.len() as u32);
        }
        self.queue.submit([encoder.finish()]);
        self.queue.present(frame);
        true
    }
}
