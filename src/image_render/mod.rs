use egui::{ColorImage, Context, TextureHandle, TextureOptions};
use rayon::prelude::*;

use crate::frame::Frame;

#[derive(Clone, Copy, PartialEq, Default)]
pub enum Colormap {
    #[default]
    Standard,
    Grayscale,
    Inferno,
    Rocket,
    Heat,
}

impl Colormap {
    pub fn label(self) -> &'static str {
        match self {
            Colormap::Standard => "Standard",
            Colormap::Grayscale => "Grayscale",
            Colormap::Inferno => "Inferno",
            Colormap::Rocket => "Rocket",
            Colormap::Heat => "Heat",
        }
    }

    pub const ALL: &'static [Colormap] =
        &[Colormap::Standard, Colormap::Grayscale, Colormap::Inferno, Colormap::Rocket, Colormap::Heat];

    /// Sample the colormap at `t` ∈ [0, 1] and return an RGB triple.
    pub fn apply(self, t: f32) -> [u8; 3] {
        apply_colormap(t, self)
    }
}

// --- Colormap control points (position in [0,1], RGB) ---

/// black → purple → red → orange → yellow
static INFERNO: &[(f32, [u8; 3])] = &[
    (0.000, [0, 0, 4]),
    (0.125, [40, 11, 84]),
    (0.250, [101, 21, 110]),
    (0.375, [159, 42, 99]),
    (0.500, [212, 72, 66]),
    (0.625, [245, 125, 21]),
    (0.750, [250, 182, 55]),
    (0.875, [252, 230, 150]),
    (1.000, [252, 255, 164]),
];

/// black → dark red → pink → white
static ROCKET: &[(f32, [u8; 3])] = &[
    (0.000, [3, 3, 3]),
    (0.125, [42, 12, 48]),
    (0.250, [87, 22, 80]),
    (0.375, [133, 40, 96]),
    (0.500, [174, 65, 97]),
    (0.625, [209, 104, 112]),
    (0.750, [234, 151, 149]),
    (0.875, [247, 200, 189]),
    (1.000, [255, 251, 253]),
];

/// white → gray → black → red
static HEAT: &[(f32, [u8; 3])] = &[
    (0.00, [255, 255, 255]),
    (0.50, [128, 128, 128]),
    (0.75, [0, 0, 0]),
    (1.00, [255, 0, 0]),
];

// --- Texture management ---

/// Manages the egui texture for the current image frame.
///
/// Tone-maps the raw u16 pixel data to RGBA8 on the CPU using rayon, then
/// uploads to the GPU via egui's texture API.  The texture is only
/// re-generated when the frame, contrast settings, or colormap change.
pub struct ImageTexture {
    handle: Option<TextureHandle>,
    last_vmin: f32,
    last_vmax: f32,
    last_gamma_correction: f32,
    last_frame_ptr: usize,
    last_colormap: Colormap,
}

impl Default for ImageTexture {
    fn default() -> Self {
        Self {
            handle: None,
            last_vmin: f32::NAN,
            last_vmax: f32::NAN,
            last_gamma_correction: f32::NAN,
            last_frame_ptr: 0,
            last_colormap: Colormap::Standard,
        }
    }
}

impl ImageTexture {
    /// Ensure the texture is up to date with `frame`, the given contrast, and colormap.
    ///
    /// Returns the egui `TextureHandle` if available.
    pub fn update(
        &mut self,
        ctx: &Context,
        frame: &Frame,
        frame_ptr: usize,
        vmin: f32,
        vmax: f32,
        gamma_correction: f32,
        colormap: Colormap,
    ) -> Option<&TextureHandle> {
        let needs_update = frame_ptr != self.last_frame_ptr
            || vmin != self.last_vmin
            || vmax != self.last_vmax
            || gamma_correction != self.last_gamma_correction
            || colormap != self.last_colormap;

        if needs_update {
            let rgba = tone_map(
                &frame.pixels,
                frame.width,
                frame.height,
                vmin,
                vmax,
                gamma_correction,
                frame.saturation_value,
                colormap,
            );
            let color_image =
                ColorImage::from_rgba_unmultiplied([frame.width as usize, frame.height as usize], &rgba);

            match &mut self.handle {
                Some(h) => h.set(color_image, TextureOptions::NEAREST),
                None => {
                    self.handle = Some(ctx.load_texture("image", color_image, TextureOptions::NEAREST));
                }
            }

            self.last_vmin = vmin;
            self.last_vmax = vmax;
            self.last_gamma_correction = gamma_correction;
            self.last_frame_ptr = frame_ptr;
            self.last_colormap = colormap;
        }

        self.handle.as_ref()
    }
}

// --- Tone mapping ---

pub(crate) fn tone_map(
    pixels: &[u16],
    _w: u32,
    _h: u32,
    vmin: f32,
    vmax: f32,
    gamma_correction: f32,
    saturation: u16,
    colormap: Colormap,
) -> Vec<u8> {
    let range = (vmax - vmin).max(1.0);
    let mut rgba = vec![0u8; pixels.len() * 4];

    rgba.par_chunks_mut(4).zip(pixels.par_iter()).for_each(|(chunk, &v)| {
        if v >= saturation {
            chunk.copy_from_slice(&[0, 0, 0, 255]);
        } else {
            let t = ((v as f32 - vmin) / range).clamp(0.0, 1.0);
            let [r, g, b] = apply_colormap(t, colormap);
            let att = |c: u8| -> u8 { ((c as f32 / 255.0).powf(gamma_correction) * 255.0).round() as u8 };
            chunk.copy_from_slice(&[att(r), att(g), att(b), 255]);
        }
    });

    rgba
}

#[inline]
fn apply_colormap(t: f32, colormap: Colormap) -> [u8; 3] {
    match colormap {
        Colormap::Standard => {
            let g = (t * 255.0) as u8;
            [g, g, g]
        }
        Colormap::Grayscale => {
            let g = ((1.0 - t) * 255.0) as u8;
            [g, g, g]
        }
        Colormap::Inferno => lerp_colormap(t, INFERNO),
        Colormap::Rocket => lerp_colormap(t, ROCKET),
        Colormap::Heat => lerp_colormap(t, HEAT),
    }
}

/// Piecewise-linear interpolation between (position, RGB) control points.
///
/// `stops` must be sorted by position with stops[0].0 == 0.0 and stops[last].0 == 1.0.
#[inline]
fn lerp_colormap(t: f32, stops: &[(f32, [u8; 3])]) -> [u8; 3] {
    let t = t.clamp(0.0, 1.0);
    // Find the last stop whose position is <= t.
    let i = stops.partition_point(|&(s, _)| s < t).saturating_sub(1).min(stops.len() - 2);
    let (t0, c0) = stops[i];
    let (t1, c1) = stops[i + 1];
    let f = if t1 > t0 { ((t - t0) / (t1 - t0)).clamp(0.0, 1.0) } else { 1.0 };
    [
        (c0[0] as f32 + f * (c1[0] as f32 - c0[0] as f32)).round() as u8,
        (c0[1] as f32 + f * (c1[1] as f32 - c0[1] as f32)).round() as u8,
        (c0[2] as f32 + f * (c1[2] as f32 - c0[2] as f32)).round() as u8,
    ]
}
