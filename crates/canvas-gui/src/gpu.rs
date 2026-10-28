//! The GPU: device, window surface (Mailbox when available, our own pacing),
//! the quad pipeline, glyphon text per layer, and offscreen rendering to PNG.

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use glyphon::{Cache, ColorMode, Resolution, SwashCache, TextArea, TextAtlas, TextBounds, TextRenderer, Viewport};
use winit::window::Window;

use crate::quads::QuadPipeline;
use crate::scene::{LAYERS, Scene};
use crate::text::{Fonts, ShapeCache};

pub struct Gpu {
    surface: wgpu::Surface<'static>,
    pub adapter: wgpu::AdapterInfo,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,
    format: wgpu::TextureFormat,
    quads: QuadPipeline,
    viewport: Viewport,
    atlas: TextAtlas,
    text: Vec<TextRenderer>,
    swash: SwashCache,
}

/// Timings of one presented frame.
#[derive(Clone, Copy, Debug, Default)]
pub struct Presented {
    /// Uploading and encoding (CPU).
    pub encode: Duration,
    /// Waiting for the swap chain image.
    pub acquire: Duration,
    /// Submit and present.
    pub present: Duration,
}

pub enum Skip {
    /// Occluded, timed out or reconfigured: try again soon.
    Retry,
}

impl Gpu {
    pub fn new(window: Arc<Window>, display: winit::event_loop::OwnedDisplayHandle) -> Result<Self> {
        let mut desc = wgpu::InstanceDescriptor::new_with_display_handle(Box::new(display));
        desc.backends = wgpu::Backends::DX12;
        let instance = wgpu::Instance::new(desc);
        let surface = instance.create_surface(window.clone()).context("cannot create the window surface")?;
        let warp = std::env::var_os("WGPU_FORCE_FALLBACK_ADAPTER").is_some_and(|v| v == "1");
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            force_fallback_adapter: warp,
            compatible_surface: Some(&surface),
            ..Default::default()
        }))
        .context("no GPU adapter")?;
        // Small allocator blocks: the default reserves ~180 MB up front.
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor { memory_hints: wgpu::MemoryHints::MemoryUsage, ..Default::default() }))
            .context("cannot open the GPU device")?;
        let caps = surface.get_capabilities(&adapter);
        let format = caps.formats.iter().copied().find(|f| !f.is_srgb()).unwrap_or(caps.formats[0]);
        // Mailbox: presenting never blocks the UI thread; frames are paced by us.
        let present_mode = [wgpu::PresentMode::Mailbox, wgpu::PresentMode::Immediate]
            .into_iter()
            .find(|m| caps.present_modes.contains(m) && std::env::var("CANVAS_GUI_PRESENT").map_or(true, |v| v != "fifo"))
            .unwrap_or(wgpu::PresentMode::Fifo);
        let size = window.inner_size();
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width: size.width.max(1),
            height: size.height.max(1),
            present_mode,
            alpha_mode: caps.alpha_modes[0],
            view_formats: vec![],
            desired_maximum_frame_latency: if present_mode == wgpu::PresentMode::Fifo { 1 } else { 2 },
            color_space: wgpu::SurfaceColorSpace::Auto,
        };
        surface.configure(&device, &config);
        let cache = Cache::new(&device);
        let viewport = Viewport::new(&device, &cache);
        // Colours are sRGB values written to a non-sRGB target.
        let mut atlas = TextAtlas::with_color_mode(&device, &queue, &cache, format, ColorMode::Web);
        let text = (0..LAYERS).map(|_| TextRenderer::new(&mut atlas, &device, wgpu::MultisampleState::default(), None)).collect();
        let quads = QuadPipeline::new(&device, format);
        Ok(Self { surface, adapter: adapter.get_info(), device, queue, config, format, quads, viewport, atlas, text, swash: SwashCache::new() })
    }

    pub fn present_mode(&self) -> wgpu::PresentMode {
        self.config.present_mode
    }

    pub fn resize(&mut self, width: u32, height: u32) {
        if width == 0 || height == 0 || (width, height) == (self.config.width, self.config.height) {
            return;
        }
        self.config.width = width;
        self.config.height = height;
        self.surface.configure(&self.device, &self.config);
    }

    /// Uploads the scene and encodes its draws into `target`.
    fn encode(&mut self, target: &wgpu::TextureView, scene: &Scene, cache: &ShapeCache, fonts: &mut Fonts) -> wgpu::CommandBuffer {
        let (width, height) = (scene.width.max(1), scene.height.max(1));
        let mut all = Vec::with_capacity(scene.quads());
        let mut ranges = Vec::with_capacity(LAYERS);
        for layer in &scene.layers {
            let a = all.len() as u32;
            all.extend_from_slice(&layer.under);
            let b = all.len() as u32;
            all.extend_from_slice(&layer.over);
            ranges.push((a..b, b..all.len() as u32));
        }
        self.quads.upload(&self.device, &self.queue, &all, width, height);
        self.viewport.update(&self.queue, Resolution { width, height });
        for (renderer, layer) in self.text.iter_mut().zip(&scene.layers) {
            let areas = layer.texts.iter().filter_map(|t| {
                cache.buffer(t.key).map(|buffer| TextArea {
                    buffer,
                    left: t.x,
                    top: t.y,
                    scale: t.scale,
                    bounds: TextBounds { left: t.clip[0].floor() as i32, top: t.clip[1].floor() as i32, right: t.clip[2].ceil() as i32, bottom: t.clip[3].ceil() as i32 },
                    default_color: crate::theme::text_color(t.color),
                    custom_glyphs: &[],
                })
            });
            if renderer.prepare(&self.device, &self.queue, &mut fonts.system, &mut self.atlas, &self.viewport, areas, &mut self.swash).is_err() {
                // The atlas is full: drop unused glyphs; the next frame retries.
                self.atlas.trim();
            }
        }
        let mut encoder = self.device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("frame") });
        {
            let [r, g, b] = scene.clear;
            let clear = wgpu::Color { r: r as f64 / 255.0, g: g as f64 / 255.0, b: b as f64 / 255.0, a: 1.0 };
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("frame"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: target,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations { load: wgpu::LoadOp::Clear(clear), store: wgpu::StoreOp::Store },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            for (i, (under, over)) in ranges.into_iter().enumerate() {
                self.quads.draw(&mut pass, under);
                if !scene.layers[i].texts.is_empty() {
                    let _ = self.text[i].render(&self.atlas, &self.viewport, &mut pass);
                }
                self.quads.draw(&mut pass, over);
            }
        }
        encoder.finish()
    }

    /// Draws the scene to the window and presents it.
    pub fn present(&mut self, window: &Window, scene: &Scene, cache: &ShapeCache, fonts: &mut Fonts) -> Result<Presented, Skip> {
        let started = Instant::now();
        let frame = match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(frame) | wgpu::CurrentSurfaceTexture::Suboptimal(frame) => frame,
            wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Lost => {
                self.surface.configure(&self.device, &self.config);
                return Err(Skip::Retry);
            }
            _ => return Err(Skip::Retry),
        };
        let acquire = started.elapsed();
        if (frame.texture.width(), frame.texture.height()) != (scene.width, scene.height) {
            // The window changed size since the scene was built.
            drop(frame);
            return Err(Skip::Retry);
        }
        let encoding = Instant::now();
        let view = frame.texture.create_view(&wgpu::TextureViewDescriptor::default());
        let commands = self.encode(&view, scene, cache, fonts);
        let encode = encoding.elapsed();
        let presenting = Instant::now();
        self.queue.submit(Some(commands));
        window.pre_present_notify();
        self.queue.present(frame);
        self.atlas.trim();
        Ok(Presented { encode, acquire, present: presenting.elapsed() })
    }

    /// Renders the scene offscreen; returns RGBA rows.
    pub fn capture(&mut self, scene: &Scene, cache: &ShapeCache, fonts: &mut Fonts) -> Result<Vec<u8>> {
        let (width, height) = (scene.width.max(1), scene.height.max(1));
        let texture = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("capture"),
            size: wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: self.format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let commands = self.encode(&view, scene, cache, fonts);
        let row = (width * 4).div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT) * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
        let buffer = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("capture"),
            size: (row * height) as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut encoder = self.device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("capture") });
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo { texture: &texture, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
            wgpu::TexelCopyBufferInfo { buffer: &buffer, layout: wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(row), rows_per_image: Some(height) } },
            wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
        );
        self.queue.submit([commands, encoder.finish()]);
        let (tx, rx) = std::sync::mpsc::channel();
        buffer.slice(..).map_async(wgpu::MapMode::Read, move |result| {
            let _ = tx.send(result);
        });
        let _ = self.device.poll(wgpu::PollType::wait_indefinitely());
        rx.recv().context("the capture was not mapped")?.context("cannot read the capture")?;
        let bgra = matches!(self.format, wgpu::TextureFormat::Bgra8Unorm | wgpu::TextureFormat::Bgra8UnormSrgb);
        let mut rgba = Vec::with_capacity((width * height * 4) as usize);
        {
            let data = buffer.slice(..).get_mapped_range().map_err(|e| anyhow::anyhow!("cannot read the capture: {e:?}"))?;
            for y in 0..height as usize {
                let line = &data[y * row as usize..y * row as usize + width as usize * 4];
                for px in line.as_chunks::<4>().0 {
                    if bgra {
                        rgba.extend_from_slice(&[px[2], px[1], px[0], 255]);
                    } else {
                        rgba.extend_from_slice(&[px[0], px[1], px[2], 255]);
                    }
                }
            }
        }
        buffer.unmap();
        Ok(rgba)
    }
}

/// Writes RGBA rows as a PNG file.
pub fn write_png(path: &std::path::Path, width: u32, height: u32, rgba: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let file = std::fs::File::create(path).with_context(|| format!("cannot create {}", path.display()))?;
    let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), width, height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header()?;
    writer.write_image_data(rgba)?;
    writer.finish()?;
    Ok(())
}
