//! The zero-copy presentation path: what the library draws, taken straight to a window.
//!
//! The compute bridge in [`crate::wgpu_bridge`] cannot serve this, and the reason is structural rather
//! than an omission. Its kernels read and write a *storage buffer*, which is the right target for
//! measurement — it is byte for byte the host surface, it needs no row padding, and it keeps the
//! integer arithmetic the conformance test compares against. But a storage buffer has no route to a
//! screen. The only way it reaches one is a download into host memory followed by whatever the
//! platform does with those bytes, and a download is exactly what this path exists to delete.
//!
//! So this module is a second, deliberately narrower pipeline. A window hands back a *texture*, the
//! source picture is uploaded once into a texture of its own, and a render pass samples the one into
//! the other. Nothing is read back. On a 4032×3024 photograph the measured path moved 48 771 072
//! bytes — the whole picture, twice, because it was uploaded and downloaded — and the view moves the
//! same bytes once, on arrival, and never again while the picture is on screen. Rescaling a picture it
//! already holds moves none at all.
//!
//! Six decisions, each against a friendlier alternative.
//!
//! **A render pass, not a compute dispatch.** The kernels above are compute because a glyph run has to
//! read the pixel it is about to blend and a render pass cannot read its own attachment. A resample
//! reads one texture and writes another, which is what a render pass is for, so it is written as one.
//! The consequence is honest and worth stating: the sampler's filtering is the driver's, not the
//! integer reference's, so this path is *not* byte-exact and is not offered as a measured pipeline.
//! It is a display path. The conformance rule still holds where it was written to hold.
//!
//! **The swapchain format is chosen without sRGB.** The source bytes are already eight-bit sRGB, taken
//! from a `Bitmap` or a `CGImage` and never converted. Writing them to an sRGB attachment would make
//! the driver encode them a second time and the picture would come out washed. The format is therefore
//! the first the surface offers that is not sRGB, and only when a surface offers none is the sRGB one
//! taken and the shift admitted.
//!
//! **One upload per source, and a redraw costs none.** `set_source` owns a texture; `draw` samples it
//! and uploads nothing. A view that is resized, scrolled, or asked for a different crop of the picture
//! it is already showing re-renders without moving a byte, which is where the path earns its name.
//!
//! **One texture per picture, however many views ask for it.** A view handed a picture uploads it
//! once; a second view handed the *same* picture borrows the texture the first one made and moves
//! nothing at all. Measured on six views of one image that is one upload and one texture instead of
//! six of each, and the picture is held on the device once rather than six times. What names a picture
//! here is the integer its caller already uses — the identifier the module answered when it decoded or
//! resampled it — which is why the identifier is an argument rather than something this module invents.
//! A borrowed texture is dropped when the last view showing it lets go, so a list that scrolls past a
//! thousand thumbnails does not leave a thousand textures behind it.
//!
//! **One device, and every view shares it.** A view that built its own `Instance`, adapter and device
//! was the first shape of this module, and the one that was measured against a real screen rather than
//! reasoned about: six views cost six device builds — between 59 and 222 ms each, 744 ms in total — six
//! copies of the same picture uploaded, and 15 MB of graphics memory apiece, for 91.9 MB that showed
//! one image six times. A view is meant to be a list item. So the expensive objects are built once for
//! the process and lent to every view, and a view now owns only what is genuinely its own: its surface,
//! its swapchain, its pipeline for the format that surface settled on, and its uniform. The rule this
//! replaces was "a shared device is a shared driver queue and a view that stalls would then stall the
//! others" — true, and the wrong trade: a driver queue is not what stalls, and the measured cost of not
//! sharing is two orders of magnitude larger than the cost of sharing it.
//!
//! **The window pointer is the caller's to keep alive.** `wgpu` gives no way to express this, so it is
//! written down here: the surface borrows the window for as long as the presenter lives, and the
//! caller releases the presenter before it releases the window. On Android that means
//! `ANativeWindow_release` happens after [`Presenter`] is dropped, never before.

use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::{Arc, Mutex};
// Only the Android handle is wrapped in a `NonNull`: the layer an Apple view hands over is passed
// through as the bare pointer `wgpu` accepts, so nothing on that side names the type.
#[cfg(target_os = "android")]
use std::ptr::NonNull;

use erl_raster_core::geom::{Rect, Size};
use erl_raster_core::{RasterError, Result};

use crate::wgpu_bridge::{platform, resolve};

/// Where the picture is drawn.
///
/// One variant per platform rather than a trait or a generic. There are exactly two, each is a bare
/// pointer, and the difference is a `match` at one call site; a trait would have to be written twice
/// and then kept from drifting, which is the failure this workspace spends more words on than any
/// other. Each variant is compiled only where it can be used, so a handle for the wrong window cannot
/// be named, let alone reached with.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum WindowHandle {
    /// An `ANativeWindow*`, as `ANativeWindow_fromSurface` hands one back.
    #[cfg(target_os = "android")]
    AndroidNdk(*mut c_void),
    /// A `CAMetalLayer*`.
    #[cfg(target_vendor = "apple")]
    CoreAnimationLayer(*mut c_void),
}

/// How the source is sampled onto the view.
///
/// The same two the ABI exposes, spelled once here rather than converted at every boundary.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum PresentFilter {
    /// Takes the nearest texel. Wrong for a photograph, right for a magnified pixel grid.
    Nearest,
    /// Blends the four neighbours. What a scaled picture wants.
    Bilinear,
}

/// What one [`Presenter::draw`] moved and whether it had to wait.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
pub struct Presented {
    /// The bytes this call uploaded. Zero when the picture was already on the device, which is the
    /// common case and the point of the module.
    pub uploaded_bytes: u64,
    /// Whether the call blocked. Acquiring a swapchain image can, and a caller that wants to know
    /// whether it paid that cost is asking a fair question.
    pub synchronised: bool,
}

/// The objects every view shares: one graphics device, its queue, the pictures uploaded so far, and
/// the pipeline pieces that do not depend on which window is being drawn into.
///
/// Built once, on the first view that asks, and kept for the life of the process. The adapter is part
/// of it rather than dropped after the device was built, because a surface's capabilities are asked of
/// the adapter and a new view has a new surface to ask about.
///
/// The render pipeline *does* depend on the window, through the swapchain format the window settled
/// on, so one is kept per format seen. In practice there is one; keeping the map costs a `Vec` and
/// removes the only per-view driver compilation that was left.
struct GpuContext {
    instance: wgpu::Instance,
    adapter: wgpu::Adapter,
    device: wgpu::Device,
    queue: wgpu::Queue,
    shader: wgpu::ShaderModule,
    pipeline_layout: wgpu::PipelineLayout,
    bind_group_layout: wgpu::BindGroupLayout,
    pipelines: Mutex<Vec<(wgpu::TextureFormat, wgpu::RenderPipeline)>>,
    /// The uploaded pictures, by the identifier their caller named them with.
    ///
    /// Held here rather than in each view because a picture is a property of the process and not of
    /// the window showing it: two views showing one photograph are two windows and one photograph.
    /// What keeps a texture alive is the `Arc` a view holds, not the entry in this map, so an entry
    /// whose picture no view is showing any more can be dropped without pulling it out from under
    /// anybody.
    sources: Mutex<HashMap<u64, Arc<Source>>>,
    sampler_nearest: wgpu::Sampler,
    sampler_bilinear: wgpu::Sampler,
}

/// The process's graphics device, or nothing before the first view asked for it.
///
/// A `Mutex` around an `Option` rather than a `OnceLock`, because the build can fail and a `OnceLock`
/// cannot be filled lazily by a fallible function without unstable API. A poisoned lock is recovered
/// rather than propagated: the only thing that can panic inside is a driver call, and a view that
/// refuses to be built is the answer the caller already handles.
static SHARED: Mutex<Option<Arc<GpuContext>>> = Mutex::new(None);

/// The shared context, building it if this is the first view.
fn shared() -> Result<Arc<GpuContext>> {
    let mut held = SHARED.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(context) = held.as_ref() {
        return Ok(Arc::clone(context));
    }
    let context = Arc::new(GpuContext::create()?);
    *held = Some(Arc::clone(&context));
    Ok(context)
}

impl GpuContext {
    /// Builds the device every view will draw with.
    ///
    /// No surface is named when the adapter is asked for, and that is the one thing sharing costs: an
    /// adapter chosen for the machine rather than for a particular window. The old code named the
    /// window so the answer could not be an adapter that cannot present to it. The name cannot be
    /// given here, because at this moment there may be no window yet and the answer must be the same
    /// for all of them. What replaces the guarantee is a check at the point it can still be made: a
    /// surface whose capabilities come back empty is refused by [`Presenter::create`] with the same
    /// error a caller already handles for an unusable window.
    fn create() -> Result<Self> {
        let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor {
            backends: platform::BACKENDS,
            ..Default::default()
        });

        let adapter = resolve(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            force_fallback_adapter: false,
            compatible_surface: None,
        }))
        .ok_or_else(|| unavailable("the adapter request did not resolve"))?
        .ok_or_else(|| unavailable(platform::NO_ADAPTER))?;

        let descriptor = wgpu::DeviceDescriptor {
            label: Some("erl-raster device"),
            required_features: wgpu::Features::empty(),
            required_limits: adapter.limits(),
            memory_hints: wgpu::MemoryHints::default(),
        };
        let requested = resolve(adapter.request_device(&descriptor, None))
            .ok_or_else(|| unavailable(platform::REQUEST_UNRESOLVED))?;
        let (device, queue) = requested.map_err(|_| unavailable(platform::DEVICE_UNCREATED))?;

        let bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("erl-raster view layout"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("erl-raster view shader"),
            source: wgpu::ShaderSource::Wgsl(std::borrow::Cow::Borrowed(SHADER)),
        });

        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("erl-raster view"),
            bind_group_layouts: &[&bind_group_layout],
            push_constant_ranges: &[],
        });

        let sampler = |label, filter| {
            device.create_sampler(&wgpu::SamplerDescriptor {
                label: Some(label),
                address_mode_u: wgpu::AddressMode::ClampToEdge,
                address_mode_v: wgpu::AddressMode::ClampToEdge,
                address_mode_w: wgpu::AddressMode::ClampToEdge,
                mag_filter: filter,
                min_filter: filter,
                mipmap_filter: wgpu::FilterMode::Nearest,
                ..Default::default()
            })
        };
        // Built before the struct so the closure's borrow of `device` ends here: the field below
        // moves the device, and a borrow cannot outlive its move.
        let sampler_nearest = sampler("erl-raster view nearest", wgpu::FilterMode::Nearest);
        let sampler_bilinear = sampler("erl-raster view bilinear", wgpu::FilterMode::Linear);

        Ok(Self {
            instance,
            adapter,
            device,
            queue,
            shader,
            pipeline_layout,
            bind_group_layout,
            pipelines: Mutex::new(Vec::new()),
            sources: Mutex::new(HashMap::new()),
            sampler_nearest,
            sampler_bilinear,
        })
    }

    /// The render pipeline for a swapchain format, built the first time that format is seen.
    fn pipeline(&self, format: wgpu::TextureFormat) -> wgpu::RenderPipeline {
        let mut held = self.pipelines.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some((_, pipeline)) = held.iter().find(|(cached, _)| *cached == format) {
            return pipeline.clone();
        }
        let pipeline = self.build_pipeline(format);
        held.push((format, pipeline.clone()));
        pipeline
    }

    /// Compiles the colour target for one format. The shader is already the device's, so what is left
    /// is the state the driver needs to bind it to an attachment of that format.
    fn build_pipeline(&self, format: wgpu::TextureFormat) -> wgpu::RenderPipeline {
        self.device
            .create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("erl-raster view"),
                layout: Some(&self.pipeline_layout),
                vertex: wgpu::VertexState {
                    module: &self.shader,
                    entry_point: Some("vertex_main"),
                    compilation_options: wgpu::PipelineCompilationOptions::default(),
                    // The quad is generated from the vertex index, so there is no buffer to describe.
                    buffers: &[],
                },
                primitive: wgpu::PrimitiveState {
                    topology: wgpu::PrimitiveTopology::TriangleStrip,
                    strip_index_format: None,
                    ..Default::default()
                },
                depth_stencil: None,
                multisample: wgpu::MultisampleState::default(),
                fragment: Some(wgpu::FragmentState {
                    module: &self.shader,
                    entry_point: Some("fragment_main"),
                    compilation_options: wgpu::PipelineCompilationOptions::default(),
                    targets: &[Some(wgpu::ColorTargetState {
                        format,
                        // The picture is premultiplied and the pass writes every pixel it covers, so
                        // there is nothing underneath to blend it over and nothing to blend.
                        blend: None,
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                }),
                multiview: None,
                cache: None,
            })
    }
}

/// One uploaded picture.
///
/// The texture is kept whole rather than only its view, because the view is what the bind group
/// captures and the texture is what keeps it alive. They are built together and replaced together.
///
/// Shared through an `Arc` rather than owned by a view, because a picture is not a view's property: a
/// list showing one photograph in six tiles is one upload and six borrowings, and the texture has to
/// outlive whichever view happened to ask for it first.
struct Source {
    texture: wgpu::Texture,
    size: Size,
}

/// The presentation pipeline for one window, drawing with the device every view shares.
///
/// What is owned here is what is genuinely per-window: the surface, the swapchain configuration the
/// surface settled on, the uniform that carries this view's rectangles, and a share of whichever
/// picture this view was told to show. Everything else — the device, the queue, the shader, the
/// layouts, the samplers, the per-format pipeline, the uploaded textures — is the shared context's,
/// which is what makes a view cheap enough to be a list item rather than a one-off.
pub struct Presenter {
    context: Arc<GpuContext>,
    surface: wgpu::Surface<'static>,
    configuration: wgpu::SurfaceConfiguration,
    pipeline: wgpu::RenderPipeline,
    uniform: wgpu::Buffer,
    source: Option<Arc<Source>>,
}

impl core::fmt::Debug for Presenter {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Presenter")
            // The device, the queue and the surface are platform objects with no useful textual form,
            // and making them printable to satisfy a log line would be the wrong trade. What a reader
            // of a report needs is the format the swapchain settled on and how big the view is.
            .field("format", &self.configuration.format)
            .field("width", &self.configuration.width)
            .field("height", &self.configuration.height)
            .field("source", &self.source.as_ref().map(|source| source.size))
            .finish()
    }
}

impl Presenter {
    /// Builds the pipeline over a window.
    ///
    /// `size` is the window's size in physical pixels and must not be zero: a zero-sized swapchain is
    /// refused by the driver, and a view that has not been laid out yet has no size to give. The
    /// caller therefore builds the presenter from the surface's own size-changed callback, not from
    /// its creation one.
    ///
    /// The size is checked before anything is built, and in particular before the shared device is
    /// asked for, so that a view with no extent costs nothing and a machine with no graphics device at
    /// all is never asked for one on behalf of a view that was going to be refused anyway.
    ///
    /// # Safety
    ///
    /// The window behind `window` must stay valid until this value is dropped, and on Android that
    /// means the caller keeps its own reference to the `ANativeWindow` for that long.
    pub unsafe fn create(window: WindowHandle, size: Size) -> Result<Self> {
        if size.is_empty() {
            return Err(RasterError::SizeOutOfRange {
                width: size.width,
                height: size.height,
            });
        }

        let context = shared()?;

        // Safety: the caller promised the window behind the handle outlives the value built here.
        let surface = unsafe { context.instance.create_surface_unsafe(surface_target(window)?) }
            .map_err(|_| unavailable("the window could not be used as a drawing surface"))?;

        let capabilities = surface.get_capabilities(&context.adapter);
        // An empty format list is how the adapter says it cannot draw into this window at all. That is
        // the check that took the place of naming the surface when the adapter was requested.
        let Some(format) = capabilities
            .formats
            .iter()
            .copied()
            .find(|format| !format.is_srgb())
            .or_else(|| capabilities.formats.first().copied())
        else {
            return Err(unavailable("the adapter cannot draw into this window"));
        };

        let configuration = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width: size.width,
            height: size.height,
            // `Fifo` is the only mode a surface is required to support, and it is the right one for a
            // view: it paces the picture to the panel instead of drawing frames nobody will see.
            present_mode: wgpu::PresentMode::Fifo,
            alpha_mode: capabilities
                .alpha_modes
                .first()
                .copied()
                .unwrap_or(wgpu::CompositeAlphaMode::Auto),
            view_formats: Vec::new(),
            desired_maximum_frame_latency: 2,
        };
        surface.configure(&context.device, &configuration);

        let pipeline = context.pipeline(format);
        let uniform = context.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("erl-raster view params"),
            size: UNIFORM_BYTES,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        Ok(Self {
            context,
            surface,
            configuration,
            pipeline,
            uniform,
            source: None,
        })
    }

    /// The size of the view, in physical pixels.
    #[must_use]
    pub fn size(&self) -> Size {
        Size::new(self.configuration.width, self.configuration.height)
    }

    /// The size of the picture currently uploaded, or `None` when there is none.
    #[must_use]
    pub fn source_size(&self) -> Option<Size> {
        self.source.as_ref().map(|source| source.size)
    }

    /// Resizes the swapchain.
    ///
    /// A no-op when the size has not changed, because reconfiguring a surface that is still valid
    /// discards its images, and a view that reconfigures on every layout pass would flicker.
    pub fn resize(&mut self, size: Size) -> Result<()> {
        if size.is_empty() {
            return Err(RasterError::SizeOutOfRange {
                width: size.width,
                height: size.height,
            });
        }
        if size == self.size() {
            return Ok(());
        }
        self.configuration.width = size.width;
        self.configuration.height = size.height;
        self.surface.configure(&self.context.device, &self.configuration);
        Ok(())
    }

    /// Shows a picture in this view, uploading it only if the device does not hold it already.
    ///
    /// `identifier` is the name the caller gave the picture — the integer its module answered when it
    /// decoded or resampled it — and `pixels` is a packed RGBA8 buffer of exactly `size.byte_len()`
    /// bytes. Two views handed the same identifier at the same size share one texture: the second one
    /// borrows the first one's and uploads nothing at all.
    ///
    /// The return value is the number of bytes this call uploaded, so a caller can tell the two cases
    /// apart and say which happened rather than assuming. Zero means the picture was already there.
    ///
    /// This is the one place the path touches host memory, and even here only when the picture is new
    /// to the device. A caller changing only the crop or the view's size must call [`Presenter::draw`]
    /// and not this.
    ///
    /// # Errors
    ///
    /// A zero-sized picture, or a buffer shorter than the size it is claimed to hold, is refused.
    pub fn set_source(&mut self, identifier: u64, size: Size, pixels: &[u8]) -> Result<u64> {
        if size.is_empty() || pixels.len() < size.byte_len() as usize {
            return Err(RasterError::SizeOutOfRange {
                width: size.width,
                height: size.height,
            });
        }

        // The lock is held across the upload rather than taken twice, and that is deliberate: two
        // views asking for the same new picture at nearly the same moment would otherwise both find
        // the map empty and both upload it, which is the cost this key exists to remove.
        let mut held = self
            .context
            .sources
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        // The picture is already on the device, at the size this caller means. The texture is
        // borrowed rather than rebuilt, and the bytes are not looked at: this is the branch that makes
        // the second view of a photograph cost nothing.
        if let Some(shared) = held.get(&identifier) {
            if shared.size == size {
                self.source = Some(Arc::clone(shared));
                return Ok(0);
            }
        }

        let texture = self.context.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("erl-raster source"),
            size: extent(size),
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            // `Rgba8Unorm` is a texture no surface is configured with, and that is the point: the
            // bytes go in and come out unchanged, with no transfer function in between.
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        // `write_texture` has copied out of this slice by the time it returns, so borrowing the
        // caller's own memory here costs nothing and saves the whole picture being copied into a
        // buffer of this module's own first, which is what the earlier shape of this function did.
        self.context.queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            pixels,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                // A packed surface is a row of exactly the width, so this is the picture's own row
                // length rather than a padded destination stride. No 256-byte alignment enters here:
                // that requirement belongs to a buffer-to-texture copy, and this is not one.
                bytes_per_row: Some(size.stride() as u32),
                rows_per_image: Some(size.height),
            },
            extent(size),
        );

        // A picture no view is showing any more is let go before the new one is filed. That is what
        // keeps this map the size of the screen rather than the size of the list: an entry is either a
        // picture some view holds, or a picture nobody does, and only the second kind is dropped. The
        // count is one more than the views, because the map itself holds one.
        held.retain(|_, entry| Arc::strong_count(entry) > 1);

        let source = Arc::new(Source { texture, size });
        held.insert(identifier, Arc::clone(&source));
        self.source = Some(source);

        Ok(size.byte_len())
    }

    /// Forgets the uploaded picture, so the view draws its background alone.
    pub fn clear_source(&mut self) {
        self.source = None;
    }

    /// Draws the picture into the view and presents it.
    ///
    /// `region` is the part of the picture to show, in the picture's own coordinates and clipped to
    /// it; `destination` is where it lands in the view, in the view's coordinates and clipped to it.
    /// `background` is what the rest of the view is filled with, as straight (not premultiplied)
    /// eight-bit channels, which is what a caller thinks in.
    ///
    /// The whole point of the function is what it does *not* do: it never reads a pixel back. The
    /// source texture is already on the device, the view's image is the driver's, and the only traffic
    /// is the draw.
    pub fn draw(
        &mut self,
        region: Rect,
        destination: Rect,
        filter: PresentFilter,
        background: [u8; 4],
    ) -> Result<Presented> {
        let Some(source) = self.source.as_ref() else {
            return self.present_only(background);
        };
        let view_size = self.size();

        // Both rectangles are clipped, and a crop that falls entirely outside the picture is a legal
        // thing to ask for — it is what a caller scrolling past a picture produces. The view is still
        // cleared and presented, because the frame that came before must not be left on screen.
        let Some(visible) = region.intersect(Rect::from_size(source.size)) else {
            return self.present_only(background);
        };
        let Some(landing) = destination.intersect(Rect::from_size(view_size)) else {
            return self.present_only(background);
        };

        let params = uniform_bytes(
            // The crop, as a normalised range. The right and bottom edges are exclusive, and dividing
            // by the size rather than by `size - 1` is what keeps the sampled span exactly as wide as
            // the region: the other divisor would stretch the last texel outward by one pixel.
            [
                visible.x as f32 / source.size.width as f32,
                visible.y as f32 / source.size.height as f32,
                visible.right() as f32 / source.size.width as f32,
                visible.bottom() as f32 / source.size.height as f32,
            ],
            [
                landing.x as f32,
                landing.y as f32,
                landing.width as f32,
                landing.height as f32,
            ],
            [view_size.width as f32, view_size.height as f32],
        );
        self.context.queue.write_buffer(&self.uniform, 0, &params);

        // The filter is the sampler's, so a nearest draw needs its own binding. Rebuilding it costs
        // one descriptor set, which is nothing next to the picture it belongs to.
        let view = source
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        let sampler = match filter {
            PresentFilter::Nearest => &self.context.sampler_nearest,
            PresentFilter::Bilinear => &self.context.sampler_bilinear,
        };
        let bind_group = self.context.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("erl-raster view"),
            layout: &self.context.bind_group_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: self.uniform.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(&view),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::Sampler(sampler),
                },
            ],
        });

        let frame = self.acquire()?;
        let target = frame
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = self
            .context
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("erl-raster view"),
            });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("erl-raster view"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &target,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(clear_color(background)),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });
            pass.set_pipeline(&self.pipeline);
            pass.set_bind_group(0, &bind_group, &[]);
            pass.draw(0..4, 0..1);
        }
        self.context.queue.submit(Some(encoder.finish()));
        frame.present();

        Ok(Presented::default())
    }

    /// Clears the view and presents it, without a picture.
    fn present_only(&mut self, background: [u8; 4]) -> Result<Presented> {
        let frame = self.acquire()?;
        let target = frame
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = self
            .context
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("erl-raster view clear"),
            });
        {
            // The pass is opened and dropped without a draw: its only work is the clear, and the
            // attachment's own load operation performs it.
            let _pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("erl-raster view clear"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &target,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(clear_color(background)),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });
        }
        self.context.queue.submit(Some(encoder.finish()));
        frame.present();
        Ok(Presented::default())
    }

    /// Takes the next swapchain image, reconfiguring once if the driver says the surface went stale.
    fn acquire(&mut self) -> Result<wgpu::SurfaceTexture> {
        match self.surface.get_current_texture() {
            Ok(frame) => Ok(frame),
            Err(_) => {
                // A surface is invalidated by a rotation, by the window being hidden, and by the
                // driver deciding the swapchain no longer matches. All three are cured by the same
                // thing, and none of them is worth surfacing to a caller as a failure: the view is
                // about to be asked to draw again anyway.
                self.surface
                    .configure(&self.context.device, &self.configuration);
                self.surface
                    .get_current_texture()
                    .map_err(|_| unavailable("the view could not acquire an image to draw into"))
            }
        }
    }
}

/// The shader, written once and used by both platforms.
///
/// It is a quad generated from the vertex index — four vertices, no buffer — positioned from the
/// destination rectangle and textured from the source rectangle. The uniform carries only what the
/// vertex stage needs; the sampler does the filtering, so the fragment stage is one `textureSample`.
const SHADER: &str = r#"
struct Params {
    // The part of the source to show, normalised: min x, min y, max x, max y.
    source_uv: vec4<f32>,
    // Where it lands in the view, in physical pixels: x, y, width, height.
    target_rect: vec4<f32>,
    // The view, in physical pixels.
    target_size: vec2<f32>,
    // Fills the uniform out to its 16-byte alignment. The shader never reads it.
    padding: vec2<f32>,
};

@group(0) @binding(0) var<uniform> params: Params;
@group(0) @binding(1) var source_texture: texture_2d<f32>;
@group(0) @binding(2) var source_sampler: sampler;

struct VertexOutput {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vertex_main(@builtin(vertex_index) index: u32) -> VertexOutput {
    // A triangle strip of four vertices, in the order (0,0), (1,0), (0,1), (1,1).
    let corner = vec2<f32>(f32(index & 1u), f32((index >> 1u) & 1u));
    let pixel = params.target_rect.xy + corner * params.target_rect.zw;
    var out: VertexOutput;
    // Device pixels to clip space. The y flip is not an accident of one platform: every window system
    // this draws into puts its origin at the top left and clip space puts it at the bottom left.
    out.position = vec4<f32>(
        pixel.x / params.target_size.x * 2.0 - 1.0,
        1.0 - pixel.y / params.target_size.y * 2.0,
        0.0,
        1.0,
    );
    out.uv = mix(params.source_uv.xy, params.source_uv.zw, corner);
    return out;
}

@fragment
fn fragment_main(in: VertexOutput) -> @location(0) vec4<f32> {
    return textureSample(source_texture, source_sampler, in.uv);
}
"#;

/// The uniform's size in bytes: three vectors and one padding pair, all `f32`.
const UNIFORM_BYTES: u64 = 48;

/// The three-dimensional extent a size describes.
///
/// A texture is a volume and a surface is not, so the depth is filled in here rather than at each of
/// the two call sites. One layer, which is what a two-dimensional texture has.
fn extent(size: Size) -> wgpu::Extent3d {
    wgpu::Extent3d {
        width: size.width,
        height: size.height,
        depth_or_array_layers: 1,
    }
}

/// Packs the three vectors the shader reads into the uniform's bytes.
///
/// Written by hand rather than derived, because deriving it would mean a serialisation crate in a
/// workspace whose only third-party dependency is the graphics API itself. Twelve `f32` values are not
/// a reason to change that.
fn uniform_bytes(source_uv: [f32; 4], target_rect: [f32; 4], target_size: [f32; 2]) -> [u8; 48] {
    let mut values = [0f32; 12];
    values[0..4].copy_from_slice(&source_uv);
    values[4..8].copy_from_slice(&target_rect);
    values[8..10].copy_from_slice(&target_size);

    let mut bytes = [0u8; 48];
    for (index, value) in values.iter().enumerate() {
        bytes[index * 4..index * 4 + 4].copy_from_slice(&value.to_ne_bytes());
    }
    bytes
}

/// Turns a straight eight-bit colour into the clear colour the pass wants.
///
/// The division is by 255 and not by 256, because the value is the top of its range rather than the
/// start of a bucket. The alpha multiplies the channels here and nowhere else: the pass blends
/// nothing, so an opaque colour has to arrive premultiplied for the result to be what the caller
/// meant.
fn clear_color(straight: [u8; 4]) -> wgpu::Color {
    let alpha = f64::from(straight[3]) / 255.0;
    wgpu::Color {
        r: f64::from(straight[0]) / 255.0 * alpha,
        g: f64::from(straight[1]) / 255.0 * alpha,
        b: f64::from(straight[2]) / 255.0 * alpha,
        a: alpha,
    }
}

/// Builds the raw handle pair `wgpu` needs to make a surface.
///
/// # Safety
///
/// The pointer inside `window` must be a live window of the kind its variant claims to be.
unsafe fn surface_target(window: WindowHandle) -> Result<wgpu::SurfaceTargetUnsafe> {
    match window {
        #[cfg(target_os = "android")]
        WindowHandle::AndroidNdk(pointer) => {
            let Some(pointer) = NonNull::new(pointer) else {
                return Err(unavailable("the window handle was null"));
            };
            Ok(wgpu::SurfaceTargetUnsafe::RawHandle {
                // Android has no separate display object: the system composer owns the display and a
                // window is all an application is given. The handle is therefore empty by definition,
                // not unknown.
                raw_display_handle: wgpu::rwh::RawDisplayHandle::Android(
                    wgpu::rwh::AndroidDisplayHandle::new(),
                ),
                raw_window_handle: wgpu::rwh::RawWindowHandle::AndroidNdk(
                    wgpu::rwh::AndroidNdkWindowHandle::new(pointer),
                ),
            })
        }
        #[cfg(target_vendor = "apple")]
        WindowHandle::CoreAnimationLayer(pointer) => {
            if pointer.is_null() {
                return Err(unavailable("the layer handle was null"));
            }
            // `wgpu` takes the layer directly here rather than through a raw handle, and a layer is
            // what the platform's own view hands out, so there is no reason to route around it.
            Ok(wgpu::SurfaceTargetUnsafe::CoreAnimationLayer(pointer))
        }
    }
}

/// A refusal, with the shape every other one in this crate has.
fn unavailable(reason: &'static str) -> RasterError {
    RasterError::BackendUnavailable {
        backend: platform::ID.as_str(),
        reason,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A handle of this platform's kind, pointing at nothing.
    fn absent_window() -> WindowHandle {
        #[cfg(target_os = "android")]
        {
            WindowHandle::AndroidNdk(std::ptr::null_mut())
        }
        #[cfg(target_vendor = "apple")]
        {
            WindowHandle::CoreAnimationLayer(std::ptr::null_mut())
        }
    }

    #[test]
    fn the_uniform_is_exactly_what_the_shader_declares() {
        // Twelve floats in three vectors, laid out with no gaps: `vec4` aligns to sixteen and `vec2`
        // to eight, so the padding is where the shader's declaration puts it. If either number moved,
        // the shader would read the wrong rectangle and the view would show the wrong part of the
        // picture, which is why it is asserted rather than assumed.
        assert_eq!(UNIFORM_BYTES, 48);
        let bytes = uniform_bytes([0.25, 0.5, 0.75, 1.0], [1.0, 2.0, 3.0, 4.0], [64.0, 32.0]);
        assert_eq!(bytes.len(), 48);

        let first = f32::from_ne_bytes(bytes[0..4].try_into().expect("four bytes"));
        let ninth = f32::from_ne_bytes(bytes[32..36].try_into().expect("four bytes"));
        assert_eq!(first, 0.25);
        assert_eq!(ninth, 64.0);
        // The padding is written as zero, and the shader is written never to read it.
        assert_eq!(&bytes[40..48], &[0u8; 8]);
    }

    #[test]
    fn an_opaque_background_is_not_scaled_by_its_own_alpha() {
        let color = clear_color([255, 128, 0, 255]);
        assert_eq!(color.a, 1.0);
        assert!((color.r - 1.0).abs() < f64::EPSILON);
        assert!((color.g - 128.0 / 255.0).abs() < 1e-12);
    }

    #[test]
    fn a_window_handle_that_is_not_there_is_refused_rather_than_followed() {
        // Safety: the pointer is null, which is precisely the case the function answers.
        let refusal = unsafe { surface_target(absent_window()) };
        assert!(matches!(refusal, Err(RasterError::BackendUnavailable { .. })));
    }

    #[test]
    fn a_view_with_no_size_has_nothing_to_configure() {
        // Safety: the window is never read, and neither is the shared device — the size is refused
        // before either, which is also what keeps this test runnable on a machine with no graphics
        // device at all.
        let refusal = unsafe { Presenter::create(absent_window(), Size::ZERO) };
        assert!(matches!(refusal, Err(RasterError::SizeOutOfRange { .. })));
    }
}
