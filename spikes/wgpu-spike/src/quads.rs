//! Instanced SDF pipeline for everything that is not text: rectangles, rounded
//! rectangles with borders, and (dashed) line segments, each clipped to a
//! rectangle.

use bytemuck::{Pod, Zeroable};

#[repr(C)]
#[derive(Clone, Copy, Debug, Pod, Zeroable)]
pub struct Quad {
    /// Rect: x, y, w, h. Segment: x0, y0, x1, y1 (physical px).
    pub rect: [f32; 4],
    pub color: [f32; 4],
    pub border: [f32; 4],
    /// radius (or segment width), border width, kind (0 rect, 1 segment), dash length (0 solid)
    pub params: [f32; 4],
    /// x0, y0, x1, y1
    pub clip: [f32; 4],
}

pub fn rgba(c: u32, a: f32) -> [f32; 4] {
    [((c >> 16) & 0xff) as f32 / 255.0, ((c >> 8) & 0xff) as f32 / 255.0, (c & 0xff) as f32 / 255.0, a]
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
            let inner = clamp(0.5 - (d + bw), 0.0, 1.0);
            col = mix(in.border, in.color, inner);
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
    let dash = in.params.w;
    if (dash > 0.0) {
        let along = h * len;
        if (fract(along / (dash * 1.7)) > 0.6) {
            alpha = 0.0;
        }
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
