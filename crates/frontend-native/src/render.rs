//! The GPU side: a `wgpu` surface, the emulated framebuffer as a texture, and `egui` on top.
//!
//! # Why the picture is drawn as an `egui` image
//!
//! The obvious alternative is a dedicated render pass that blits the framebuffer, with `egui`
//! drawn over it in a second pass. That was rejected for two reasons. Compositing order becomes
//! the frontend's problem — every panel, tooltip, and menu has to be drawn after the game and
//! before nothing else — and the destination rectangle stops being the same number the layout code
//! computed, so the touch mapping and the drawing would be free to disagree.
//!
//! Registering the framebuffer as a native texture and drawing it through `egui`'s painter gives
//! correct compositing for free, uses exactly the rectangle [`crate::layout`] produced, and leaves
//! this module with one job: keep a texture the size of the current framebuffer, and put this
//! frame's bytes in it.
//!
//! The scaling *mode* still lives here, because a filter is a sampler property: nearest and
//! integer-nearest differ only in the rectangle [`crate::layout`] chooses, while bilinear differs
//! only in the sampler. Both halves are needed and neither belongs in the other's module.
//!
//! # `Rgba8Unorm`, deliberately
//!
//! `egui-wgpu` documents that user textures must be `Rgba8Unorm` and stores its own that way. Its
//! shader picks an entry point based on whether the surface format is sRGB, with the effect that
//! byte values pass through unchanged. The core's framebuffer is already sRGB-encoded `RGBA8`, so
//! `Rgba8Unorm` is what makes the colours come out as the PPU computed them; `Rgba8UnormSrgb`
//! would apply the transfer function a second time and wash the picture out.

use anyhow::{Context, Result};
use egui_wgpu::wgpu;
use frontend_core::{Frame, ScalingMode};
use std::sync::Arc;
use winit::window::Window;

use crate::block_on::block_on;
use crate::layout::Layout;

/// Owns the surface and the emulator texture.
pub struct Renderer {
    painter: egui_wgpu::winit::Painter,
    screen: Option<ScreenTexture>,
}

/// The emulator framebuffer on the GPU.
struct ScreenTexture {
    texture: wgpu::Texture,
    id: egui::TextureId,
    width: u32,
    height: u32,
    /// The sampler baked into the registered bind group. Changing the filter means registering
    /// again, which is why it is remembered rather than passed in each frame.
    filter: wgpu::FilterMode,
}

impl Renderer {
    /// Create the painter. No device is chosen until [`set_window`](Self::set_window).
    pub fn new(context: egui::Context) -> Self {
        // `WgpuConfiguration::default()` sets up an instance with no display handle. That is fine
        // on macOS, Windows, and most Linux setups; Wayland-with-GLES is the case that wants
        // `WgpuSetup::from_display_handle`, which needs a handle that outlives the instance and
        // `ActiveEventLoop` does not provide one.
        let painter = block_on(egui_wgpu::winit::Painter::new(
            context,
            egui_wgpu::WgpuConfiguration::default(),
            false,
            egui_wgpu::RendererOptions {
                // egui already anti-aliases by feathering, and there is no 3D here to smooth.
                msaa_samples: 0,
                ..Default::default()
            },
        ));
        Self {
            painter,
            screen: None,
        }
    }

    /// Attach the surface. Must be called before anything is drawn.
    pub fn set_window(&mut self, window: Arc<Window>) -> Result<()> {
        block_on(
            self.painter
                .set_window(egui::ViewportId::ROOT, Some(window)),
        )
        .context("could not create a GPU surface for the window")?;
        Ok(())
    }

    pub fn on_window_resized(&mut self, width: u32, height: u32) {
        // Zero in either axis means a minimised window; wgpu rejects a surface of that size, and
        // there is nothing to draw into anyway.
        if let (Some(width), Some(height)) = (
            std::num::NonZeroU32::new(width),
            std::num::NonZeroU32::new(height),
        ) {
            self.painter
                .on_window_resized(egui::ViewportId::ROOT, width, height);
        }
    }

    /// The largest texture the device will accept, once one has been chosen.
    pub fn max_texture_side(&self) -> Option<usize> {
        self.painter.max_texture_side()
    }

    pub fn adapter_summary(&self) -> String {
        match self.painter.render_state() {
            Some(state) => egui_wgpu::adapter_info_summary(&state.adapter.get_info()),
            None => "no GPU adapter yet".to_string(),
        }
    }

    /// Put this frame's pixels on the GPU, returning the texture to draw.
    ///
    /// Recreates the texture when the framebuffer changes size — which happens when the user
    /// switches from a Game Boy to a Game Boy Advance — and re-registers it when the scaling mode
    /// changes the sampler. Neither happens in the steady state, so the per-frame cost is one
    /// `write_texture`.
    pub fn upload(&mut self, frame: &Frame, mode: ScalingMode) -> Option<egui::TextureId> {
        let state = self.painter.render_state()?;
        let width = frame.buffer.width();
        let height = frame.buffer.height();
        if width == 0 || height == 0 {
            return None;
        }
        let filter = match mode {
            ScalingMode::Linear => wgpu::FilterMode::Linear,
            ScalingMode::Nearest | ScalingMode::IntegerNearest => wgpu::FilterMode::Nearest,
        };

        let needs_new_texture = match &self.screen {
            Some(screen) => screen.width != width || screen.height != height,
            None => true,
        };
        if needs_new_texture {
            if let Some(old) = self.screen.take() {
                state.renderer.write().free_texture(&old.id);
            }
            let texture = state.device.create_texture(&wgpu::TextureDescriptor {
                label: Some("emulator framebuffer"),
                size: wgpu::Extent3d {
                    width,
                    height,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Rgba8Unorm,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            });
            let id = register(&state, &texture, filter);
            self.screen = Some(ScreenTexture {
                texture,
                id,
                width,
                height,
                filter,
            });
        } else if self.screen.as_ref().is_some_and(|s| s.filter != filter) {
            let screen = self.screen.as_mut().expect("checked above");
            state.renderer.write().free_texture(&screen.id);
            screen.id = register(&state, &screen.texture, filter);
            screen.filter = filter;
        }

        let screen = self.screen.as_ref()?;
        state.queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &screen.texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            frame.buffer.as_bytes(),
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(width * 4),
                rows_per_image: Some(height),
            },
            wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
        );
        Some(screen.id)
    }

    /// Forget the uploaded frame, so closing a ROM does not leave its last frame on screen.
    pub fn clear_screen_texture(&mut self) {
        if let (Some(state), Some(screen)) = (self.painter.render_state(), self.screen.take()) {
            state.renderer.write().free_texture(&screen.id);
        }
    }

    /// Draw one frame: the tessellated `egui` output, which includes the emulator image.
    pub fn paint(
        &mut self,
        window: &Arc<Window>,
        pixels_per_point: f32,
        primitives: &[egui::epaint::ClippedPrimitive],
        textures_delta: &egui::epaint::textures::TexturesDelta,
    ) {
        self.painter.paint_and_update_textures(
            egui::ViewportId::ROOT,
            pixels_per_point,
            // Near-black rather than black, so the letterbox is visibly *the application* and not
            // a dead region of the screen.
            [0.02, 0.02, 0.03, 1.0],
            primitives,
            textures_delta,
            Vec::new(),
            window,
        );
    }
}

fn register(
    state: &egui_wgpu::RenderState,
    texture: &wgpu::Texture,
    filter: wgpu::FilterMode,
) -> egui::TextureId {
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    state
        .renderer
        .write()
        .register_native_texture_with_sampler_options(
            &state.device,
            &view,
            wgpu::SamplerDescriptor {
                label: Some("emulator framebuffer sampler"),
                mag_filter: filter,
                // Minification only happens when the window is smaller than the emulated screen,
                // which for a 160×144 Game Boy essentially never is. Matching `mag_filter` keeps
                // the two consistent if it does.
                min_filter: filter,
                mipmap_filter: wgpu::MipmapFilterMode::Nearest,
                // Clamping matters at the edges under bilinear filtering: repeating would blend
                // the left column into the right one and put a seam down both sides.
                address_mode_u: wgpu::AddressMode::ClampToEdge,
                address_mode_v: wgpu::AddressMode::ClampToEdge,
                address_mode_w: wgpu::AddressMode::ClampToEdge,
                ..Default::default()
            },
        )
}

/// Draw the emulated screens into an `egui` painter at the rectangles the layout chose.
///
/// Kept as a free function rather than a method so it needs nothing from [`Renderer`]: the
/// rectangles come from [`crate::layout`], the texture id from [`Renderer::upload`], and the
/// drawing is `egui`'s.
pub fn draw_screens(
    painter: &egui::Painter,
    layout: &Layout,
    texture: egui::TextureId,
    framebuffer: (u32, u32),
) {
    let (fb_width, fb_height) = framebuffer;
    if fb_width == 0 || fb_height == 0 {
        return;
    }
    // A hairline around the whole picture. Against the near-black letterbox, a dark game scene
    // otherwise has no visible edge at all, and on the DS it is what makes two screens plus a gap
    // read as one device rather than two floating rectangles.
    if let Some(bounds) = layout.bounds() {
        painter.rect_stroke(
            egui::Rect::from_min_size(
                egui::pos2(bounds.x, bounds.y),
                egui::vec2(bounds.width, bounds.height),
            ),
            0.0,
            egui::Stroke::new(1.0, egui::Color32::from_gray(60)),
            egui::StrokeKind::Outside,
        );
    }
    for screen in &layout.screens {
        // UV coordinates address the region of the framebuffer this screen shows, which is how one
        // texture serves the DS's two stacked screens without a second upload.
        let uv = egui::Rect::from_min_max(
            egui::pos2(
                screen.source.x as f32 / fb_width as f32,
                screen.source.y as f32 / fb_height as f32,
            ),
            egui::pos2(
                (screen.source.x + screen.source.width) as f32 / fb_width as f32,
                (screen.source.y + screen.source.height) as f32 / fb_height as f32,
            ),
        );
        let dest = egui::Rect::from_min_size(
            egui::pos2(screen.dest.x, screen.dest.y),
            egui::vec2(screen.dest.width, screen.dest.height),
        );
        painter.image(texture, dest, uv, egui::Color32::WHITE);
    }
}
