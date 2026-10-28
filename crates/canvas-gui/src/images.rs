//! Images on the GPU: a textured-rectangle pipeline and a texture cache keyed
//! by image (terminal images by their serial, the paste preview by its own).
//! Textures not drawn for [`EVICT_AFTER`] are freed, and the least recently
//! drawn go first when the cache passes [`MAX_TEXTURE_BYTES`].

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytemuck::{Pod, Zeroable};
use canvas_core::protocol::{IMAGE_PNG, IMAGE_RGBA, ServiceMsg};

use crate::scene::ImageItem;

/// Textures unused this long are freed.
pub const EVICT_AFTER: Duration = Duration::from_secs(20);
/// Texture memory kept at most.
pub const MAX_TEXTURE_BYTES: usize = 256 << 20;
/// Largest image decoded (width × height).
const MAX_PIXELS: u64 = 4096 * 4096;

#[repr(C)]
#[derive(Clone, Copy, Debug, Pod, Zeroable)]
struct Instance {
    rect: [f32; 4],
    uv: [f32; 4],
    clip: [f32; 4],
}

const SHADER: &str = r#"
struct Globals { size: vec2<f32>, pad: vec2<f32> };
@group(0) @binding(0) var<uniform> globals: Globals;
@group(0) @binding(1) var samp: sampler;
@group(1) @binding(0) var tex: texture_2d<f32>;

struct Inst {
    @location(0) rect: vec4<f32>,
    @location(1) uv: vec4<f32>,
    @location(2) clip: vec4<f32>,
};

struct VOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
    @location(1) p: vec2<f32>,
    @location(2) clip: vec4<f32>,
};

@vertex
fn vs(@builtin(vertex_index) vi: u32, inst: Inst) -> VOut {
    let corner = vec2<f32>(f32(vi & 1u), f32(vi >> 1u));
    let p = inst.rect.xy + inst.rect.zw * corner;
    var out: VOut;
    out.pos = vec4<f32>(p.x / globals.size.x * 2.0 - 1.0, 1.0 - p.y / globals.size.y * 2.0, 0.0, 1.0);
    out.uv = mix(inst.uv.xy, inst.uv.zw, corner);
    out.p = p;
    out.clip = inst.clip;
    return out;
}

@fragment
fn fs(in: VOut) -> @location(0) vec4<f32> {
    if (in.p.x < in.clip.x || in.p.y < in.clip.y || in.p.x >= in.clip.z || in.p.y >= in.clip.w) {
        discard;
    }
    return textureSampleLevel(tex, samp, in.uv, 0.0);
}
"#;

struct Texture {
    bind_group: wgpu::BindGroup,
    bytes: usize,
    used: Instant,
    _texture: wgpu::Texture,
}

pub struct ImagePipeline {
    pipeline: wgpu::RenderPipeline,
    globals: wgpu::Buffer,
    globals_group: wgpu::BindGroup,
    texture_layout: wgpu::BindGroupLayout,
    instances: wgpu::Buffer,
    capacity: usize,
    textures: HashMap<u64, Texture>,
    /// Instances of the current frame, in drawing order.
    queued: Vec<(u64, Instance)>,
}

impl ImagePipeline {
    pub fn new(device: &wgpu::Device, format: wgpu::TextureFormat) -> Self {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor { label: Some("images"), source: wgpu::ShaderSource::Wgsl(SHADER.into()) });
        let globals = device.create_buffer(&wgpu::BufferDescriptor { label: Some("image globals"), size: 16, usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST, mapped_at_creation: false });
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("images"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        let globals_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("image globals"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::VERTEX,
                    ty: wgpu::BindingType::Buffer { ty: wgpu::BufferBindingType::Uniform, has_dynamic_offset: false, min_binding_size: None },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry { binding: 1, visibility: wgpu::ShaderStages::FRAGMENT, ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering), count: None },
            ],
        });
        let globals_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("image globals"),
            layout: &globals_layout,
            entries: &[wgpu::BindGroupEntry { binding: 0, resource: globals.as_entire_binding() }, wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::Sampler(&sampler) }],
        });
        let texture_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("image texture"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Texture { sample_type: wgpu::TextureSampleType::Float { filterable: true }, view_dimension: wgpu::TextureViewDimension::D2, multisampled: false },
                count: None,
            }],
        });
        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("images"),
            bind_group_layouts: &[Some(&globals_layout), Some(&texture_layout)],
            ..Default::default()
        });
        let attributes = wgpu::vertex_attr_array![0 => Float32x4, 1 => Float32x4, 2 => Float32x4];
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("images"),
            layout: Some(&layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs"),
                buffers: &[Some(wgpu::VertexBufferLayout { array_stride: std::mem::size_of::<Instance>() as u64, step_mode: wgpu::VertexStepMode::Instance, attributes: &attributes })],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs"),
                targets: &[Some(wgpu::ColorTargetState { format, blend: Some(wgpu::BlendState::ALPHA_BLENDING), write_mask: wgpu::ColorWrites::ALL })],
                compilation_options: Default::default(),
            }),
            primitive: wgpu::PrimitiveState { topology: wgpu::PrimitiveTopology::TriangleStrip, ..Default::default() },
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            cache: None,
            multiview_mask: None,
        });
        let capacity = 64;
        let instances = Self::buffer(device, capacity);
        Self { pipeline, globals, globals_group, texture_layout, instances, capacity, textures: HashMap::new(), queued: Vec::new() }
    }

    fn buffer(device: &wgpu::Device, capacity: usize) -> wgpu::Buffer {
        device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("image instances"),
            size: (capacity * std::mem::size_of::<Instance>()) as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        })
    }

    /// Textures in the cache (tests and the hooks).
    pub fn textures(&self) -> usize {
        self.textures.len()
    }

    /// Queues an image for this frame (uploading its texture if needed);
    /// returns its index for [`ImagePipeline::draw`].
    pub fn queue(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, item: &ImageItem) -> Option<u32> {
        if item.width == 0 || item.height == 0 || item.rgba.len() < item.width as usize * item.height as usize * 4 {
            return None;
        }
        let now = Instant::now();
        if let Some(texture) = self.textures.get_mut(&item.key) {
            texture.used = now;
        } else {
            let size = wgpu::Extent3d { width: item.width, height: item.height, depth_or_array_layers: 1 };
            let texture = device.create_texture(&wgpu::TextureDescriptor {
                label: Some("image"),
                size,
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Rgba8Unorm,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            });
            queue.write_texture(
                wgpu::TexelCopyTextureInfo { texture: &texture, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
                &item.rgba[..item.width as usize * item.height as usize * 4],
                wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(item.width * 4), rows_per_image: Some(item.height) },
                size,
            );
            let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
            let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("image"),
                layout: &self.texture_layout,
                entries: &[wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(&view) }],
            });
            self.textures.insert(item.key, Texture { bind_group, bytes: (item.width * item.height * 4) as usize, used: now, _texture: texture });
        }
        self.queued.push((item.key, Instance { rect: item.rect, uv: item.uv, clip: item.clip }));
        Some(self.queued.len() as u32 - 1)
    }

    /// Uploads the queued instances (once per frame, before drawing).
    pub fn upload(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, width: u32, height: u32) {
        queue.write_buffer(&self.globals, 0, bytemuck::cast_slice(&[width as f32, height as f32, 0.0, 0.0]));
        if self.queued.len() > self.capacity {
            self.capacity = self.queued.len().next_power_of_two();
            self.instances = Self::buffer(device, self.capacity);
        }
        if !self.queued.is_empty() {
            let instances: Vec<Instance> = self.queued.iter().map(|(_, i)| *i).collect();
            queue.write_buffer(&self.instances, 0, bytemuck::cast_slice(&instances));
        }
    }

    /// Draws queued images `range`.
    pub fn draw(&self, pass: &mut wgpu::RenderPass<'_>, range: std::ops::Range<u32>) {
        if range.is_empty() {
            return;
        }
        pass.set_pipeline(&self.pipeline);
        pass.set_bind_group(0, &self.globals_group, &[]);
        pass.set_vertex_buffer(0, self.instances.slice(..));
        for index in range {
            let Some(texture) = self.queued.get(index as usize).and_then(|(key, _)| self.textures.get(key)) else { continue };
            pass.set_bind_group(1, &texture.bind_group, &[]);
            pass.draw(0..4, index..index + 1);
        }
    }

    /// Ends a frame: forgets its instances and frees textures not drawn lately.
    pub fn end_frame(&mut self) {
        self.queued.clear();
        let now = Instant::now();
        self.textures.retain(|_, t| now.duration_since(t.used) < EVICT_AFTER);
        let mut total: usize = self.textures.values().map(|t| t.bytes).sum();
        while total > MAX_TEXTURE_BYTES {
            let Some((&oldest, _)) = self.textures.iter().min_by_key(|(_, t)| t.used) else { break };
            total -= self.textures.remove(&oldest).map(|t| t.bytes).unwrap_or(0);
        }
    }
}

/// A PNG file as straight RGBA rows; `None` when it does not decode or is too large.
pub fn decode_png(bytes: &[u8]) -> Option<(u32, u32, Vec<u8>)> {
    let mut decoder = png::Decoder::new(std::io::Cursor::new(bytes));
    decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut reader = decoder.read_info().ok()?;
    let (width, height) = (reader.info().width, reader.info().height);
    if width == 0 || height == 0 || width as u64 * height as u64 > MAX_PIXELS {
        return None;
    }
    let mut buffer = vec![0; reader.output_buffer_size()?];
    let frame = reader.next_frame(&mut buffer).ok()?;
    let pixels = &buffer[..frame.buffer_size()];
    let rgba: Vec<u8> = match frame.color_type {
        png::ColorType::Rgba => pixels.to_vec(),
        png::ColorType::Rgb => pixels.as_chunks::<3>().0.iter().flat_map(|p| [p[0], p[1], p[2], 255]).collect(),
        png::ColorType::GrayscaleAlpha => pixels.as_chunks::<2>().0.iter().flat_map(|p| [p[0], p[0], p[0], p[1]]).collect(),
        png::ColorType::Grayscale => pixels.iter().flat_map(|&g| [g, g, g, 255]).collect(),
        png::ColorType::Indexed => return None,
    };
    (rgba.len() == width as usize * height as usize * 4).then_some((width, height, rgba))
}

/// Decodes a PNG image message to RGBA off the UI thread (the forwarding
/// thread calls this); images that do not decode are dropped.
pub fn decode_message(message: ServiceMsg) -> Option<ServiceMsg> {
    match message {
        ServiceMsg::Image { term, image_id, rgba_or_png, format: IMAGE_PNG, .. } => {
            let (width, height, rgba) = decode_png(&rgba_or_png)?;
            Some(ServiceMsg::Image { term, image_id, width, height, rgba_or_png: rgba, format: IMAGE_RGBA })
        }
        other => Some(other),
    }
}

/// A smaller copy of RGBA pixels that fits in `max_w` × `max_h` (nearest neighbour).
pub fn thumbnail(width: u32, height: u32, rgba: &[u8], max_w: u32, max_h: u32) -> (u32, u32, Arc<Vec<u8>>) {
    let scale = (max_w as f32 / width as f32).min(max_h as f32 / height as f32).min(1.0);
    let (tw, th) = (((width as f32 * scale).round() as u32).max(1), ((height as f32 * scale).round() as u32).max(1));
    let mut out = Vec::with_capacity((tw * th * 4) as usize);
    for y in 0..th {
        let sy = ((y as f32 + 0.5) / scale) as u32;
        for x in 0..tw {
            let sx = ((x as f32 + 0.5) / scale) as u32;
            let i = ((sy.min(height - 1) * width + sx.min(width - 1)) * 4) as usize;
            out.extend_from_slice(&rgba[i..i + 4]);
        }
    }
    (tw, th, Arc::new(out))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn png_messages_are_decoded_to_rgba() {
        let rgba = vec![10, 20, 30, 255, 40, 50, 60, 128];
        let mut png = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut png, 2, 1);
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Eight);
            encoder.write_header().unwrap().write_image_data(&rgba).unwrap();
        }
        let message = ServiceMsg::Image { term: "t".into(), image_id: 4, width: 0, height: 0, rgba_or_png: png, format: IMAGE_PNG };
        let Some(ServiceMsg::Image { width, height, rgba_or_png, format, .. }) = decode_message(message) else { panic!("decoded") };
        assert_eq!((width, height, format, rgba_or_png), (2, 1, IMAGE_RGBA, rgba));
        let broken = ServiceMsg::Image { term: "t".into(), image_id: 4, width: 0, height: 0, rgba_or_png: vec![1, 2, 3], format: IMAGE_PNG };
        assert!(decode_message(broken).is_none());
    }

    #[test]
    fn thumbnails_keep_the_aspect_ratio() {
        let rgba: Vec<u8> = (0..100 * 50).flat_map(|i| [(i % 256) as u8, 0, 0, 255]).collect();
        let (w, h, pixels) = thumbnail(100, 50, &rgba, 40, 40);
        assert_eq!((w, h, pixels.len()), (40, 20, 40 * 20 * 4));
        let (w, h, _) = thumbnail(10, 10, &rgba[..400], 40, 40);
        assert_eq!((w, h), (10, 10), "never enlarged");
    }
}
