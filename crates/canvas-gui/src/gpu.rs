//! The GPU: device, window surfaces (Mailbox when available, our own pacing),
//! the quad pipeline, glyphon text, and offscreen rendering to PNG.
//!
//! Every window keeps its scene layers on the GPU between frames (quad
//! instances and prepared text per layer), so a frame in which only the
//! terminals changed uploads and prepares just those layers. A lost device
//! (driver reset, removed adapter) is reported through [`Gpu::lost`]; the
//! shell then builds everything again with [`Gpu::new`].

use std::ops::Range;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use glyphon::{Cache, ColorMode, Resolution, SwashCache, TextArea, TextAtlas, TextBounds, TextRenderer, Viewport};
use winit::window::Window;

use crate::images::ImagePipeline;
use crate::quads::{Quad, QuadPipeline};
use crate::scene::{LAYERS, Scene};
use crate::text::{Fonts, ShapeCache};

/// Which scene layers changed since the window's last frame.
pub type Dirty = [bool; LAYERS];
pub const ALL: Dirty = [true; LAYERS];

/// The device and everything shared by all windows: pipelines and the glyph atlas.
pub struct Gpu {
    instance: wgpu::Instance,
    adapter: wgpu::Adapter,
    pub info: wgpu::AdapterInfo,
    /// Windows' software rasterizer (WARP) stands in for a missing GPU.
    pub software: bool,
    device: wgpu::Device,
    queue: wgpu::Queue,
    format: wgpu::TextureFormat,
    quads: QuadPipeline,
    pub images: ImagePipeline,
    cache: Cache,
    atlas: TextAtlas,
    swash: SwashCache,
    /// Bumped whenever the atlas is trimmed: text prepared before may point at
    /// glyphs evicted since, so those layers are prepared again.
    atlas_gen: u64,
    /// Set by the device-lost callback or a fatal device error.
    pub lost: Arc<AtomicBool>,
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

/// One scene layer on the GPU: its quad instances and prepared text.
struct LayerGpu {
    text: TextRenderer,
    quads: wgpu::Buffer,
    capacity: usize,
    under: Range<u32>,
    over: Range<u32>,
    has_text: bool,
}

/// Layers kept between frames (a window's, or a capture's).
pub struct Layers {
    viewport: Viewport,
    layers: Vec<LayerGpu>,
    /// The atlas generation the text was prepared in (`None`: not prepared).
    prepared: Option<u64>,
}

/// One window's swap chain and layers.
pub struct Surface {
    surface: wgpu::Surface<'static>,
    config: wgpu::SurfaceConfiguration,
    layers: Option<Layers>,
}

impl Surface {
    pub fn present_mode(&self) -> wgpu::PresentMode {
        self.config.present_mode
    }

    pub fn resize(&mut self, gpu: &Gpu, width: u32, height: u32) {
        if width == 0 || height == 0 || (width, height) == (self.config.width, self.config.height) {
            return;
        }
        self.config.width = width;
        self.config.height = height;
        self.surface.configure(&gpu.device, &self.config);
    }
}

pub enum Skip {
    /// Occluded, timed out or reconfigured: try again soon.
    Retry,
    /// The device is gone: rebuild the GPU objects.
    DeviceLost,
}

/// Test switch (`CANVAS_GUI_TEST_GPU`): `nohw` refuses hardware adapters,
/// `none` every adapter, to exercise the fallbacks.
fn refused(software: bool) -> bool {
    match std::env::var("CANVAS_GUI_TEST_GPU").as_deref() {
        Ok("none") => true,
        Ok("nohw") => !software,
        _ => false,
    }
}

impl Gpu {
    /// Opens the device for the first window (its surface decides the adapter
    /// and format); later windows get theirs from `surface`. Falls back to
    /// WARP when no hardware adapter works; failures are logged with the
    /// adapter's details.
    pub fn new(window: Arc<Window>, display: winit::event_loop::OwnedDisplayHandle) -> Result<(Self, Surface)> {
        let mut desc = wgpu::InstanceDescriptor::new_with_display_handle(Box::new(display));
        desc.backends = wgpu::Backends::DX12;
        let instance = wgpu::Instance::new(desc);
        let surface = instance.create_surface(window.clone()).context("cannot create the window surface")?;
        let forced = std::env::var_os("WGPU_FORCE_FALLBACK_ADAPTER").is_some_and(|v| v == "1");
        let mut failures = Vec::new();
        for software in if forced { vec![true] } else { vec![false, true] } {
            match Self::open(&instance, &surface, software) {
                Ok((adapter, device, queue)) => {
                    let info = adapter.get_info();
                    let software = software || info.device_type == wgpu::DeviceType::Cpu;
                    if !failures.is_empty() {
                        crate::crashlog::event("gpu", &format!("drawing with {} ({:?}) after: {}", info.name, info.device_type, failures.join("; ")));
                    }
                    let gpu = Self::assemble(instance, adapter, info, software, device, queue, &surface);
                    let surface = gpu.configure(surface, &window);
                    return Ok((gpu, surface));
                }
                Err(error) => {
                    let what = if software { "software adapter (WARP)" } else { "hardware adapter" };
                    crate::crashlog::event("gpu", &format!("{what}: {error:#}"));
                    failures.push(format!("{what}: {error:#}"));
                }
            }
        }
        anyhow::bail!("{}", failures.join("; "))
    }

    fn open(instance: &wgpu::Instance, surface: &wgpu::Surface<'static>, software: bool) -> Result<(wgpu::Adapter, wgpu::Device, wgpu::Queue)> {
        anyhow::ensure!(!refused(software), "refused by CANVAS_GUI_TEST_GPU");
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            force_fallback_adapter: software,
            compatible_surface: Some(surface),
            ..Default::default()
        }))
        .context("no adapter")?;
        let info = adapter.get_info();
        // Small allocator blocks: the default reserves ~180 MB up front.
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor { memory_hints: wgpu::MemoryHints::MemoryUsage, ..Default::default() }))
            .with_context(|| format!("cannot open the device on {} ({:?}, driver {} {})", info.name, info.device_type, info.driver, info.driver_info))?;
        Ok((adapter, device, queue))
    }

    fn assemble(instance: wgpu::Instance, adapter: wgpu::Adapter, info: wgpu::AdapterInfo, software: bool, device: wgpu::Device, queue: wgpu::Queue, surface: &wgpu::Surface<'static>) -> Self {
        let lost = Arc::new(AtomicBool::new(false));
        {
            let (lost, name) = (lost.clone(), info.name.clone());
            device.set_device_lost_callback(move |reason, message| {
                crate::crashlog::event("gpu", &format!("device lost on {name}: {reason:?} {message}"));
                lost.store(true, Ordering::Release);
                crate::shell::wake();
            });
        }
        {
            let (lost, name) = (lost.clone(), info.name.clone());
            device.on_uncaptured_error(Arc::new(move |error| {
                crate::crashlog::event("gpu", &format!("error on {name}: {error}"));
                if matches!(error, wgpu::Error::OutOfMemory { .. } | wgpu::Error::Internal { .. }) {
                    lost.store(true, Ordering::Release);
                    crate::shell::wake();
                }
            }));
        }
        let caps = surface.get_capabilities(&adapter);
        let format = caps.formats.iter().copied().find(|f| !f.is_srgb()).unwrap_or(caps.formats[0]);
        let cache = Cache::new(&device);
        // Colours are sRGB values written to a non-sRGB target.
        let atlas = TextAtlas::with_color_mode(&device, &queue, &cache, format, ColorMode::Web);
        let quads = QuadPipeline::new(&device, format);
        let images = ImagePipeline::new(&device, format);
        Self { instance, adapter, info, software, device, queue, format, quads, images, cache, atlas, swash: SwashCache::new(), atlas_gen: 0, lost }
    }

    /// The adapter in one line (logs, the hooks).
    pub fn describe(&self) -> String {
        format!("{} ({:?}, {:?}, driver {} {})", self.info.name, self.info.device_type, self.info.backend, self.info.driver, self.info.driver_info)
    }

    /// A swap chain for another window on the same device.
    pub fn surface(&self, window: Arc<Window>) -> Result<Surface> {
        let surface = self.instance.create_surface(window.clone()).context("cannot create the window surface")?;
        Ok(self.configure(surface, &window))
    }

    fn configure(&self, surface: wgpu::Surface<'static>, window: &Window) -> Surface {
        let caps = surface.get_capabilities(&self.adapter);
        // Mailbox: presenting never blocks the UI thread; frames are paced by us.
        let present_mode = [wgpu::PresentMode::Mailbox, wgpu::PresentMode::Immediate]
            .into_iter()
            .find(|m| caps.present_modes.contains(m) && std::env::var("CANVAS_GUI_PRESENT").map_or(true, |v| v != "fifo"))
            .unwrap_or(wgpu::PresentMode::Fifo);
        let size = window.inner_size();
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format: self.format,
            width: size.width.max(1),
            height: size.height.max(1),
            present_mode,
            alpha_mode: caps.alpha_modes[0],
            view_formats: vec![],
            desired_maximum_frame_latency: if present_mode == wgpu::PresentMode::Fifo { 1 } else { 2 },
            color_space: wgpu::SurfaceColorSpace::Auto,
        };
        surface.configure(&self.device, &config);
        Surface { surface, config, layers: None }
    }

    fn new_layers(&mut self) -> Layers {
        let viewport = Viewport::new(&self.device, &self.cache);
        let layers = (0..LAYERS)
            .map(|_| LayerGpu {
                text: TextRenderer::new(&mut self.atlas, &self.device, wgpu::MultisampleState::default(), None),
                quads: QuadPipeline::buffer(&self.device, 256),
                capacity: 256,
                under: 0..0,
                over: 0..0,
                has_text: false,
            })
            .collect();
        Layers { viewport, layers, prepared: None }
    }

    /// Uploads the `dirty` layers of `scene` (all when the text must be
    /// prepared again) and encodes the draws into `target`. `trim`: glyphs
    /// unused since the last trim may be evicted (a full frame).
    #[allow(clippy::too_many_arguments)]
    fn encode(&mut self, target: &wgpu::TextureView, scene: &Scene, dirty: &Dirty, slot: &mut Option<Layers>, trim: bool, cache: &ShapeCache, fonts: &mut Fonts) -> wgpu::CommandBuffer {
        let mut layers = match slot.take() {
            Some(layers) => layers,
            None => self.new_layers(),
        };
        let (width, height) = (scene.width.max(1), scene.height.max(1));
        if trim {
            self.atlas.trim();
            self.atlas_gen += 1;
        }
        let stale = layers.prepared != Some(self.atlas_gen);
        self.quads.globals(&self.queue, width, height);
        layers.viewport.update(&self.queue, Resolution { width, height });
        let mut failed = false;
        for (i, (layer, gpu)) in scene.layers.iter().zip(layers.layers.iter_mut()).enumerate() {
            if !(stale || dirty[i]) {
                continue;
            }
            let count = layer.under.len() + layer.over.len();
            if count > gpu.capacity {
                gpu.capacity = count.next_power_of_two();
                gpu.quads = QuadPipeline::buffer(&self.device, gpu.capacity);
            }
            if !layer.under.is_empty() {
                self.queue.write_buffer(&gpu.quads, 0, bytemuck::cast_slice(&layer.under));
            }
            if !layer.over.is_empty() {
                let offset = (layer.under.len() * std::mem::size_of::<Quad>()) as u64;
                self.queue.write_buffer(&gpu.quads, offset, bytemuck::cast_slice(&layer.over));
            }
            gpu.under = 0..layer.under.len() as u32;
            gpu.over = layer.under.len() as u32..count as u32;
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
            gpu.has_text = !layer.texts.is_empty();
            if gpu.has_text && gpu.text.prepare(&self.device, &self.queue, &mut fonts.system, &mut self.atlas, &layers.viewport, areas, &mut self.swash).is_err() {
                // The atlas is full: the next frame trims it and prepares everything.
                failed = true;
            }
        }
        layers.prepared = (!failed).then_some(self.atlas_gen);
        // Images per layer: (below the text, above it) as index ranges.
        let mut image_ranges = Vec::with_capacity(LAYERS);
        for layer in &scene.layers {
            let mut queue_all = |items: &[crate::scene::ImageItem]| {
                let indices: Vec<u32> = items.iter().filter_map(|item| self.images.queue(&self.device, &self.queue, item)).collect();
                indices.first().copied().unwrap_or(0)..indices.last().map_or(0, |l| l + 1)
            };
            let under = queue_all(&layer.images_under);
            let over = queue_all(&layer.images_over);
            image_ranges.push((under, over));
        }
        self.images.upload(&self.device, &self.queue, width, height);
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
            for (gpu, (images_under, images_over)) in layers.layers.iter().zip(image_ranges) {
                self.quads.draw(&mut pass, &gpu.quads, gpu.under.clone());
                self.images.draw(&mut pass, images_under);
                if gpu.has_text {
                    let _ = gpu.text.render(&self.atlas, &layers.viewport, &mut pass);
                }
                self.images.draw(&mut pass, images_over);
                self.quads.draw(&mut pass, &gpu.quads, gpu.over.clone());
            }
        }
        self.images.end_frame();
        *slot = Some(layers);
        encoder.finish()
    }

    /// Draws the scene to a window (`dirty`: the layers that changed since its
    /// last frame) and presents it.
    #[allow(clippy::too_many_arguments)]
    pub fn present(&mut self, target: &mut Surface, window: &Arc<Window>, scene: &Scene, dirty: &Dirty, cache: &ShapeCache, fonts: &mut Fonts) -> Result<Presented, Skip> {
        if self.lost.load(Ordering::Acquire) {
            return Err(Skip::DeviceLost);
        }
        let started = Instant::now();
        let frame = match target.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(frame) | wgpu::CurrentSurfaceTexture::Suboptimal(frame) => frame,
            wgpu::CurrentSurfaceTexture::Outdated => {
                target.surface.configure(&self.device, &target.config);
                return Err(Skip::Retry);
            }
            wgpu::CurrentSurfaceTexture::Lost => {
                // A new surface for the same window (a monitor or driver change can cause this).
                crate::crashlog::event("gpu", &format!("surface lost on {}; recreating it", self.describe()));
                let Ok(surface) = self.instance.create_surface(window.clone()) else { return Err(Skip::DeviceLost) };
                surface.configure(&self.device, &target.config);
                target.surface = surface;
                return Err(Skip::Retry);
            }
            wgpu::CurrentSurfaceTexture::Validation => {
                crate::crashlog::event("gpu", &format!("surface validation error on {}", self.describe()));
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
        let trim = dirty.iter().all(|d| *d);
        let commands = self.encode(&view, scene, dirty, &mut target.layers, trim, cache, fonts);
        let encode = encoding.elapsed();
        let presenting = Instant::now();
        self.queue.submit(Some(commands));
        window.pre_present_notify();
        self.queue.present(frame);
        Ok(Presented { encode, acquire, present: presenting.elapsed() })
    }

    /// Renders the scene offscreen with layers of its own (the windows' stay
    /// as they are, the atlas is not trimmed); returns RGBA rows.
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
        let commands = self.encode(&view, scene, &ALL, &mut None, false, cache, fonts);
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

    /// Destroys the device: the driver reports the loss as after a reset
    /// (the `device-lost` test hook with `"destroy": true`).
    pub fn destroy(&self) {
        self.device.destroy();
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
