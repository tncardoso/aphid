//! One offscreen Blade context, and a frame of pixels out of it.
//!
//! ## Why the creature arrives as pixels
//!
//! GPUI has no public seam onto its own GPU. The renderer is `platform/blade`
//! on Linux and `platform/mac/metal_renderer` on macOS, but `mod blade` is
//! private (`platform.rs:18`) and `BladeContext` is not re-exported;
//! `Element::paint` emits the primitives of a `Scene` and nothing else, and the
//! one raw-surface primitive, `Window::paint_surface`, is macOS-only and wants
//! a `CVPixelBuffer`. So the creature is drawn in a context of this crate's
//! own, read back, and handed to GPUI as an image.
//!
//! That is a GPU round trip for every frame, and it is deliberate: at 256x256
//! it is 256 KB, and there is no other seam to use.
//!
//! ## Why the sizes are what they are
//!
//! Every width here is a multiple of 64 pixels, which at four bytes a pixel
//! makes every row a multiple of 256 bytes. That is the row alignment both
//! backends want, so the buffer that comes back is the image with no padding to
//! strip out.
//!
//! The target is `Bgra8Unorm` because GPUI's `RenderImage` reads BGRA
//! (`assets.rs:41`). The shaders write their channels in that order, so nothing
//! is swizzled on the CPU.

use blade_graphics as gpu;

use crate::gui::config::Familiar;
use crate::gui::emote::Emote;

/// What the shaders read. One uniform, four fields, no buffers.
#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct Params {
    time: f32,
    emote: u32,
    previous: u32,
    blend: f32,
}

#[derive(blade_macros::ShaderData)]
struct Data {
    params: Params,
}

/// How long to wait for one frame before giving up on it, in milliseconds.
///
/// Long enough for a machine under load, short enough that a wedged driver does
/// not take the thread with it.
const TIMEOUT: u32 = 1_000;

/// A context, a pipeline, a target and a buffer to read it back through.
pub struct Painter {
    context: gpu::Context,
    pipeline: gpu::RenderPipeline,
    target: gpu::Texture,
    view: gpu::TextureView,
    readback: gpu::Buffer,
    encoder: gpu::CommandEncoder,
    width: u32,
    height: u32,
    /// Whether the target has been initialised, which has to happen once and on
    /// the GPU timeline rather than at creation.
    started: bool,
}

impl Painter {
    /// Build a context for one familiar at one size.
    ///
    /// # Errors
    ///
    /// Fails when there is no device this can run on — no Vulkan, a driver too
    /// old, a remote session — which is the case the window is written to carry
    /// on without.
    pub fn new(familiar: Familiar, width: u32, height: u32) -> Result<Self, String> {
        // SAFETY: the context is created once and owned by this struct, which
        // is what Blade asks of the caller.
        let context = unsafe {
            gpu::Context::init(gpu::ContextDesc {
                // No window is presented to: the whole point is that GPUI owns
                // the window and this owns a texture.
                presentation: false,
                validation: false,
                timing: false,
                capture: false,
                overlay: false,
                device_id: 0,
            })
        }
        .map_err(|error| format!("no device to draw the alate on: {error:?}"))?;

        let shader = context.create_shader(gpu::ShaderDesc {
            source: match familiar {
                Familiar::Sap => include_str!("sap.wgsl"),
                Familiar::Drift => include_str!("drift.wgsl"),
            },
        });
        let format = gpu::TextureFormat::Bgra8Unorm;
        let pipeline = context.create_render_pipeline(gpu::RenderPipelineDesc {
            name: familiar.label(),
            data_layouts: &[&<Data as gpu::ShaderData>::layout()],
            vertex: shader.at("vs_main"),
            vertex_fetches: &[],
            primitive: gpu::PrimitiveState {
                // Four corners from the vertex index, so there is no vertex
                // buffer and nothing to keep in step with the shader.
                topology: gpu::PrimitiveTopology::TriangleStrip,
                ..Default::default()
            },
            depth_stencil: None,
            fragment: Some(shader.at("fs_main")),
            color_targets: &[format.into()],
            multisample_state: gpu::MultisampleState::default(),
        });

        let size = gpu::Extent {
            width,
            height,
            depth: 1,
        };
        let target = context.create_texture(gpu::TextureDesc {
            name: "alate",
            format,
            size,
            array_layer_count: 1,
            mip_level_count: 1,
            sample_count: 1,
            dimension: gpu::TextureDimension::D2,
            usage: gpu::TextureUsage::TARGET | gpu::TextureUsage::COPY,
            external: None,
        });
        let view = context.create_texture_view(
            target,
            gpu::TextureViewDesc {
                name: "alate",
                format,
                dimension: gpu::ViewDimension::D2,
                subresources: &gpu::TextureSubresources::default(),
            },
        );
        let readback = context.create_buffer(gpu::BufferDesc {
            name: "alate readback",
            size: u64::from(width) * u64::from(height) * 4,
            // Host visible, because the CPU is what reads it.
            memory: gpu::Memory::Shared,
        });
        let encoder = context.create_command_encoder(gpu::CommandEncoderDesc {
            name: "alate",
            buffer_count: 2,
        });

        Ok(Self {
            context,
            pipeline,
            target,
            view,
            readback,
            encoder,
            width,
            height,
            started: false,
        })
    }

    /// How many bytes one frame is.
    #[must_use]
    pub fn len(&self) -> usize {
        self.width as usize * self.height as usize * 4
    }

    /// Draw one frame and bring it back as BGRA bytes.
    ///
    /// `time` is seconds since the window opened, which is what the shaders
    /// animate against. `blend` crossfades out of `previous`.
    ///
    /// # Errors
    ///
    /// Fails when the GPU does not finish inside [`TIMEOUT`], or when the
    /// readback buffer is not where it should be.
    pub fn frame(
        &mut self,
        time: f32,
        emote: Emote,
        previous: Emote,
        blend: f32,
    ) -> Result<Vec<u8>, String> {
        let size = gpu::Extent {
            width: self.width,
            height: self.height,
            depth: 1,
        };
        self.encoder.start();
        if !self.started {
            // The first use of a texture has to be announced on the GPU
            // timeline, not at creation.
            self.encoder.init_texture(self.target);
            self.started = true;
        }
        {
            let mut pass = self.encoder.render(
                "alate",
                gpu::RenderTargetSet {
                    colors: &[gpu::RenderTarget {
                        view: self.view,
                        init_op: gpu::InitOp::Clear(gpu::TextureColor::TransparentBlack),
                        finish_op: gpu::FinishOp::Store,
                    }],
                    depth_stencil: None,
                },
            );
            let mut drawing = pass.with(&self.pipeline);
            drawing.bind(
                0,
                &Data {
                    params: Params {
                        time,
                        emote: emote.id(),
                        previous: previous.id(),
                        blend,
                    },
                },
            );
            drawing.draw(0, 4, 0, 1);
        }
        {
            let mut transfer = self.encoder.transfer("read back");
            transfer.copy_texture_to_buffer(
                self.target.into(),
                self.readback.into(),
                self.width * 4,
                size,
            );
        }

        let sync = self.context.submit(&mut self.encoder);
        if !self.context.wait_for(&sync, TIMEOUT) {
            return Err("the GPU did not finish a frame in a second".to_owned());
        }

        let bytes = self.readback.data();
        if bytes.is_null() {
            return Err("the readback buffer is not host visible".to_owned());
        }
        // SAFETY: the buffer is `Memory::Shared`, so its data pointer is
        // mapped for as long as the buffer lives, and the frame that filled it
        // has been waited for above.
        Ok(unsafe { std::slice::from_raw_parts(bytes, self.len()) }.to_vec())
    }
}

impl Drop for Painter {
    fn drop(&mut self) {
        self.context.destroy_command_encoder(&mut self.encoder);
        self.context.destroy_render_pipeline(&mut self.pipeline);
        self.context.destroy_texture_view(self.view);
        self.context.destroy_texture(self.target);
        self.context.destroy_buffer(self.readback);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Ignored: continuous integration has no GPU, and this needs a real one.
    /// Run it by hand with `cargo test --features gui -- --ignored`.
    #[test]
    #[ignore = "needs a GPU"]
    fn a_frame_comes_back_the_right_size_and_is_not_all_black() {
        let mut painter = Painter::new(Familiar::Sap, 256, 256).expect("a device");
        let frame = painter
            .frame(0.5, Emote::Talking, Emote::Idle, 1.)
            .expect("a frame");
        assert_eq!(frame.len(), 256 * 256 * 4);
        assert!(
            frame.iter().any(|&byte| byte > 8),
            "the alate drew nothing at all"
        );
    }

    #[test]
    #[ignore = "needs a GPU"]
    fn the_other_familiar_draws_too() {
        let mut painter = Painter::new(Familiar::Drift, 192, 192).expect("a device");
        let frame = painter
            .frame(1., Emote::Thinking, Emote::Idle, 1.)
            .expect("a frame");
        assert_eq!(frame.len(), 192 * 192 * 4);
        assert!(frame.iter().any(|&byte| byte > 8));
    }
}
