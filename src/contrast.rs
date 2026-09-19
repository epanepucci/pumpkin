use crate::frame::Frame;
use crate::image_render::Colormap;
use crate::viewport::ViewState;

/// Tone-mapping controls.
#[derive(Clone, PartialEq)]
pub struct ContrastState {
    pub vmin: f32,
    pub vmax: f32,
    pub auto: bool,
    pub colormap: Colormap,
    /// Power-law exponent applied after linear normalisation: t' = t^gamma_correction.
    /// 1.0 = linear (no change); >1.0 darkens background, preserves bright peaks.
    pub gamma_correction: f32,
    pub histogram_log: bool,
    pub histogram_bins: usize,
}

impl Default for ContrastState {
    fn default() -> Self {
        Self {
            vmin: 0.0,
            vmax: 1000.0,
            auto: true,
            colormap: Colormap::Inferno,
            gamma_correction: 1.0,
            histogram_log: true,
            histogram_bins: 256,
        }
    }
}

/// Lower / upper percentiles (of valid, non-zero pixels) used for auto contrast.
const AUTO_CONTRAST_LOW_PCT: f64 = 0.01;
const AUTO_CONTRAST_HIGH_PCT: f64 = 0.998;
/// Minimum display range so near-empty frames don't collapse to a flat image.
pub const AUTO_CONTRAST_MIN_SPAN: f32 = 5.0;

/// Percentile-based contrast over the whole frame. See `contrast_from_histogram`.
pub fn auto_contrast(frame: &Frame) -> (f32, f32) {
    contrast_from_histogram(frame, 0..frame.pixels.len())
        .unwrap_or((0.0, AUTO_CONTRAST_MIN_SPAN))
}

/// Build a histogram of the valid pixels at `indices` (unmasked and below the
/// saturation value) in one O(n) pass and derive (vmin, vmax) from percentiles
/// of the non-zero pixels. Zeros dominate diffraction frames and would pin the
/// low percentile at 0, and a percentile (unlike the maximum) is robust against
/// hot pixels. Returns `None` if there are no valid non-zero pixels.
fn contrast_from_histogram(
    frame: &Frame,
    indices: impl Iterator<Item = usize>,
) -> Option<(f32, f32)> {
    let sat = frame.saturation_value;
    let mut hist = vec![0u32; u16::MAX as usize + 1];
    let mut total = 0u64;
    for i in indices {
        let v = frame.pixels[i];
        if v != 0 && v < sat && !frame.is_masked_index(i) {
            hist[v as usize] += 1;
            total += 1;
        }
    }
    if total == 0 {
        return None;
    }

    // Smallest value whose cumulative count reaches the requested fraction.
    let percentile = |frac: f64| -> u16 {
        let target = ((total as f64 * frac).ceil() as u64).clamp(1, total);
        let mut cum = 0u64;
        for (v, &count) in hist.iter().enumerate() {
            cum += count as u64;
            if cum >= target {
                return v as u16;
            }
        }
        u16::MAX
    };

    let vmin = percentile(AUTO_CONTRAST_LOW_PCT) as f32;
    let vmax = (percentile(AUTO_CONTRAST_HIGH_PCT) as f32).max(vmin + AUTO_CONTRAST_MIN_SPAN);
    Some((vmin, vmax))
}

/// Same algorithm as `auto_contrast` but restricted to the pixels currently
/// visible in the viewport. Falls back to the full-frame version if the
/// visible region has no valid (unmasked, unsaturated, non-zero) pixels.
pub fn auto_contrast_region(frame: &Frame, view: &ViewState, viewport: egui::Rect) -> (f32, f32) {
    let x0 = view.offset.x.max(0.0) as u32;
    let y0 = view.offset.y.max(0.0) as u32;
    let x1 = (view.offset.x + viewport.width() / view.zoom).min(frame.width as f32) as u32;
    let y1 = (view.offset.y + viewport.height() / view.zoom).min(frame.height as f32) as u32;

    if x1 <= x0 || y1 <= y0 {
        return auto_contrast(frame);
    }

    let indices = (y0..y1).flat_map(|y| (x0..x1).map(move |x| (y * frame.width + x) as usize));
    contrast_from_histogram(frame, indices).unwrap_or_else(|| auto_contrast(frame))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_frame(pixels: Vec<u16>, sat: u16) -> Frame {
        Frame {
            width: pixels.len() as u32,
            height: 1,
            pixels,
            pixel_mask: None,
            saturation_value: sat,
            metadata: Default::default(),
        }
    }

    #[test]
    fn auto_contrast_ignores_hot_pixel_and_zeros() {
        let mut pixels = vec![0u16; 5000];
        pixels.extend((0..4990).map(|i| 2 + (i % 8) as u16));
        pixels.extend([9000u16; 3]); // hot pixels: < 0.1% of valid pixels
        let (vmin, vmax) = auto_contrast(&test_frame(pixels, 60000));
        assert!(vmin >= 2.0 && vmin <= 3.0, "vmin={vmin}");
        assert!(vmax > vmin && vmax < 100.0, "vmax={vmax}");
    }

    #[test]
    fn auto_contrast_empty_frame_has_positive_span() {
        let (vmin, vmax) = auto_contrast(&test_frame(vec![0; 100], 60000));
        assert!(vmax > vmin);
    }
}
