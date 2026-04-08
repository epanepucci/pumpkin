use egui::{ColorImage, Context, TextureHandle, TextureOptions};
use rayon::prelude::*;

use crate::frame::Frame;

/// Manages the egui texture for the current image frame.
///
/// Tone-maps the raw u16 pixel data to RGBA8 on the CPU using rayon, then
/// uploads to the GPU via egui's texture API.  The texture is only
/// re-generated when the frame or the contrast settings change.
pub struct ImageTexture {
    handle: Option<TextureHandle>,
    last_vmin: f32,
    last_vmax: f32,
    /// Pointer identity check: we compare Arc pointers to avoid re-uploading
    /// the same frame with the same contrast settings.
    last_frame_ptr: usize,
}

impl Default for ImageTexture {
    fn default() -> Self {
        Self {
            handle: None,
            last_vmin: f32::NAN,
            last_vmax: f32::NAN,
            last_frame_ptr: 0,
        }
    }
}

impl ImageTexture {
    /// Ensure the texture is up to date with `frame` and the given contrast.
    ///
    /// Returns the egui `TextureHandle` if available.
    pub fn update(
        &mut self,
        ctx: &Context,
        frame: &Frame,
        frame_ptr: usize,
        vmin: f32,
        vmax: f32,
    ) -> Option<&TextureHandle> {
        let needs_update = frame_ptr != self.last_frame_ptr
            || vmin != self.last_vmin
            || vmax != self.last_vmax;

        if needs_update {
            let rgba = tone_map(&frame.pixels, frame.width, frame.height, vmin, vmax, frame.saturation_value);
            let color_image =
                ColorImage::from_rgba_unmultiplied([frame.width as usize, frame.height as usize], &rgba);

            match &mut self.handle {
                Some(h) => h.set(color_image, TextureOptions::NEAREST),
                None => {
                    self.handle =
                        Some(ctx.load_texture("image", color_image, TextureOptions::NEAREST));
                }
            }

            self.last_vmin = vmin;
            self.last_vmax = vmax;
            self.last_frame_ptr = frame_ptr;
        }

        self.handle.as_ref()
    }
}

/// Tone-map a u16 pixel buffer to RGBA8 using rayon for parallelism.
///
/// Saturated pixels are rendered red.  All others are linearly mapped to
/// grayscale in [vmin, vmax].
fn tone_map(pixels: &[u16], _w: u32, _h: u32, vmin: f32, vmax: f32, saturation: u16) -> Vec<u8> {
    let range = (vmax - vmin).max(1.0);
    let mut rgba = vec![0u8; pixels.len() * 4];

    rgba.par_chunks_mut(4)
        .zip(pixels.par_iter())
        .for_each(|(chunk, &v)| {
            if v >= saturation {
                chunk.copy_from_slice(&[255, 0, 0, 255]);
            } else {
                let t = ((v as f32 - vmin) / range).clamp(0.0, 1.0);
                let g = (t * 255.0) as u8;
                chunk.copy_from_slice(&[g, g, g, 255]);
            }
        });

    rgba
}
