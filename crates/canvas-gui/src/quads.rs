//! Instanced SDF pipeline for everything that is not text: rectangles,
//! rounded rectangles with (dashed) borders, and (dashed) line segments, each
//! clipped to a rectangle. Coordinates are physical pixels.

use bytemuck::{Pod, Zeroable};

#[repr(C)]
#[derive(Clone, Copy, Debug, Pod, Zeroable, PartialEq)]
pub struct Quad {
    /// Rect: x, y, w, h. Segment: x0, y0, x1, y1.
    pub rect: [f32; 4],
    /// Fill (rect) or line colour (segment), straight alpha.
    pub color: [f32; 4],
    pub border: [f32; 4],
    /// radius (segment: width), border width, kind (0 rect, 1 segment), dash period (0 solid)
    pub params: [f32; 4],
    /// x0, y0, x1, y1
    pub clip: [f32; 4],
}

/// A colour as straight-alpha floats.
pub type Rgba = [f32; 4];

#[cfg(test)]
pub const NO_CLIP: [f32; 4] = [-1e9, -1e9, 1e9, 1e9];

impl Quad {
    /// A filled rectangle.
    pub fn rect(x: f32, y: f32, w: f32, h: f32, color: Rgba, clip: [f32; 4]) -> Self {
        Self { rect: [x, y, w, h], color, border: [0.0; 4], params: [0.0; 4], clip }
    }

    /// A rounded rectangle with an optional border drawn inside its edge.
    #[allow(clippy::too_many_arguments)]
    pub fn rounded(x: f32, y: f32, w: f32, h: f32, radius: f32, fill: Rgba, border: Rgba, border_w: f32, clip: [f32; 4]) -> Self {
        Self { rect: [x, y, w, h], color: fill, border, params: [radius, border_w, 0.0, 0.0], clip }
    }

    /// The same with a dashed border (`period`: dash plus gap length).
    pub fn dashed(mut self, period: f32) -> Self {
        self.params[3] = period;
        self
    }

    /// A line segment with round caps.
    pub fn line(a: (f32, f32), b: (f32, f32), width: f32, color: Rgba, dash: f32, clip: [f32; 4]) -> Self {
        Self { rect: [a.0, a.1, b.0, b.1], color, border: [0.0; 4], params: [width, 0.0, 1.0, dash], clip }
    }
}

const SHADER: &str = r#"
struct Globals { size: vec2<f32>, pad: vec2<f32> };
@group(0) @binding(0) var<uniform> globals: Globals;

struct Inst {
    @location(0) rect: vec4<f32>,
    @location(1) color: vec4<f32>,
    @location(2) border: vec4<f32>,
    @location(3) params: vec4<f32>,
    @location(4) clip: vec4<f32>,
};

struct VOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) p: vec2<f32>,
    @location(1) color: vec4<f32>,
    @location(2) border: vec4<f32>,
    @location(3) params: vec4<f32>,
    @location(4) rect: vec4<f32>,
    @location(5) clip: vec4<f32>,
};

@vertex
fn vs(@builtin(vertex_index) vi: u32, inst: Inst) -> VOut {
    let corner = vec2<f32>(f32(vi & 1u), f32(vi >> 1u));
    var lo: vec2<f32>;
    var hi: vec2<f32>;
    if (inst.params.z < 0.5) {
        lo = inst.rect.xy;
        hi = inst.rect.xy + inst.rect.zw;
    } else {
        let hw = inst.params.x * 0.5;
        lo = min(inst.rect.xy, inst.rect.zw) - vec2<f32>(hw);
        hi = max(inst.rect.xy, inst.rect.zw) + vec2<f32>(hw);
    }
    lo = max(lo - vec2<f32>(1.0), inst.clip.xy);
    hi = min(hi + vec2<f32>(1.0), inst.clip.zw);
    hi = max(hi, lo);
    let p = mix(lo, hi, corner);
    var out: VOut;
    out.pos = vec4<f32>(p.x / globals.size.x * 2.0 - 1.0, 1.0 - p.y / globals.size.y * 2.0, 0.0, 1.0);
    out.p = p;
    out.color = inst.color;
    out.border = inst.border;
    out.params = inst.params;
    out.rect = inst.rect;
    out.clip = inst.clip;
    return out;
}

@fragment
fn fs(in: VOut) -> @location(0) vec4<f32> {
    let p = in.p;
    if (p.x < in.clip.x || p.y < in.clip.y || p.x >= in.clip.z || p.y >= in.clip.w) {
        discard;
    }
    let dash = in.params.w;
    if (in.params.z < 0.5) {
        let half = in.rect.zw * 0.5;
        let center = in.rect.xy + half;
        let r = min(in.params.x, min(half.x, half.y));
        let q = abs(p - center) - half + vec2<f32>(r);
        let d = length(max(q, vec2<f32>(0.0))) + min(max(q.x, q.y), 0.0) - r;
        let coverage = clamp(0.5 - d, 0.0, 1.0);
        var col = in.color;
        let bw = in.params.y;
        if (bw > 0.0) {
            var border = in.border;
            if (dash > 0.0) {
                // Position along the nearest edge decides dash or gap.
                let rel = p - in.rect.xy;
                let dx = min(rel.x, in.rect.z - rel.x);
                let dy = min(rel.y, in.rect.w - rel.y);
                let along = select(rel.x, rel.y, dx < dy);
                if (fract(along / dash) > 0.55) {
                    border = vec4<f32>(in.color.rgb, in.color.a);
                }
            }
            let inner = clamp(0.5 - (d + bw), 0.0, 1.0);
            col = mix(border, in.color, inner);
        }
        return vec4<f32>(col.rgb, col.a * coverage);
    }
    let a = in.rect.xy;
    let b = in.rect.zw;
    let pa = p - a;
    let ba = b - a;
    let len = max(length(ba), 0.0001);
    let h = clamp(dot(pa, ba) / (len * len), 0.0, 1.0);
    let d = length(pa - ba * h) - in.params.x * 0.5;
    var alpha = clamp(0.5 - d, 0.0, 1.0);
    if (dash > 0.0 && fract(h * len / dash) > 0.55) {
        alpha = 0.0;
    }
    return vec4<f32>(in.color.rgb, in.color.a * alpha);
}
"#;

pub struct QuadPipeline {
    pipeline: wgpu::RenderPipeline,
    globals: wgpu::Buffer,
    bind_group: wgpu::BindGroup,
    instances: wgpu::Buffer,
    capacity: usize,
}

impl QuadPipeline {
    pub fn new(device: &wgpu::Device, format: wgpu::TextureFormat) -> Self {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor { label: Some("quads"), source: wgpu::ShaderSource::Wgsl(SHADER.into()) });
        let globals = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("quad globals"),
            size: 16,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("quad globals layout"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX,
                ty: wgpu::BindingType::Buffer { ty: wgpu::BufferBindingType::Uniform, has_dynamic_offset: false, min_binding_size: None },
                count: None,
            }],
        });
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("quad globals"),
            layout: &layout,
            entries: &[wgpu::BindGroupEntry { binding: 0, resource: globals.as_entire_binding() }],
        });
        let pipeline_layout =
            device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor { label: Some("quads"), bind_group_layouts: &[Some(&layout)], ..Default::default() });
        let attributes = wgpu::vertex_attr_array![0 => Float32x4, 1 => Float32x4, 2 => Float32x4, 3 => Float32x4, 4 => Float32x4];
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("quads"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs"),
                buffers: &[Some(wgpu::VertexBufferLayout {
                    array_stride: std::mem::size_of::<Quad>() as u64,
                    step_mode: wgpu::VertexStepMode::Instance,
                    attributes: &attributes,
                })],
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
        let capacity = 16 * 1024;
        let instances = Self::buffer(device, capacity);
        QuadPipeline { pipeline, globals, bind_group, instances, capacity }
    }

    fn buffer(device: &wgpu::Device, capacity: usize) -> wgpu::Buffer {
        device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("quad instances"),
            size: (capacity * std::mem::size_of::<Quad>()) as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        })
    }

    pub fn upload(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, quads: &[Quad], width: u32, height: u32) {
        if quads.len() > self.capacity {
            self.capacity = quads.len().next_power_of_two();
            self.instances = Self::buffer(device, self.capacity);
        }
        queue.write_buffer(&self.globals, 0, bytemuck::cast_slice(&[width as f32, height as f32, 0.0, 0.0]));
        if !quads.is_empty() {
            queue.write_buffer(&self.instances, 0, bytemuck::cast_slice(quads));
        }
    }

    pub fn draw(&self, pass: &mut wgpu::RenderPass<'_>, range: std::ops::Range<u32>) {
        if range.is_empty() {
            return;
        }
        pass.set_pipeline(&self.pipeline);
        pass.set_bind_group(0, &self.bind_group, &[]);
        pass.set_vertex_buffer(0, self.instances.slice(..));
        pass.draw(0..4, range);
    }
}
