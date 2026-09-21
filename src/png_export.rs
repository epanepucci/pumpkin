use std::io::BufWriter;
use std::path::{Path, PathBuf};

use anyhow::Context;

use crate::frame::Frame;
use crate::geometry::Geometry;
use crate::image_render::{tone_map, ToneMapParams};
use crate::viewport::OverlaySettings;

/// Derive a PNG save filename from frame metadata.
pub fn derive_filename(frame: &Frame) -> String {
    let stem = frame
        .metadata
        .name_pattern
        .as_deref()
        .and_then(|p| Path::new(p).file_stem()?.to_str())
        .map(|s| s.strip_suffix("_master").unwrap_or(s))
        .unwrap_or("pumpkin");

    match frame.metadata.image_number {
        Some(n) => format!("{stem}_{n:06}.png"),
        None => format!("{stem}.png"),
    }
}

/// Tone-map `frame` and draw the enabled overlays onto the RGBA buffer.
pub fn render_rgba(frame: &Frame, params: ToneMapParams, overlays: &OverlaySettings) -> Vec<u8> {
    let w = frame.width;
    let h = frame.height;

    let mut rgba = tone_map(&frame.pixels, frame.pixel_mask.as_deref(), w, h, params);

    if overlays.show_beam_center {
        if let (Some(cx), Some(cy)) = (frame.metadata.beam_center_x, frame.metadata.beam_center_y) {
            let arm = 20.0f32;
            let cx = cx as f32;
            let cy = cy as f32;
            let sw = overlays.beam_center_stroke_width.max(1.0);
            let col = overlays.beam_center_color;
            draw_line(&mut rgba, w, h, cx - arm, cy, cx + arm, cy, col, sw);
            draw_line(&mut rgba, w, h, cx, cy - arm, cx, cy + arm, col, sw);
        }
    }

    if overlays.show_resolution_rings {
        draw_resolution_rings(&mut rgba, w, h, frame, overlays);
    }

    rgba
}

/// Output size for `percent` (clamped to 1..=100) of a `w` x `h` image; never below 1 px.
fn scaled_size(w: u32, h: u32, percent: u32) -> (u32, u32) {
    let pct = percent.clamp(1, 100) as f64 / 100.0;
    (
        ((w as f64 * pct).round() as u32).max(1),
        ((h as f64 * pct).round() as u32).max(1),
    )
}

/// Shrink an RGBA buffer to `nw` x `nh` by averaging the source pixels covered
/// by each output pixel (box filter).
fn downscale_area(rgba: Vec<u8>, w: u32, h: u32, nw: u32, nh: u32) -> Vec<u8> {
    if (nw, nh) == (w, h) {
        return rgba;
    }
    let (w, h, nw, nh) = (w as usize, h as usize, nw as usize, nh as usize);
    let mut out = Vec::with_capacity(nw * nh * 4);
    for oy in 0..nh {
        let y0 = oy * h / nh;
        let y1 = ((oy + 1) * h).div_ceil(nh).clamp(y0 + 1, h);
        for ox in 0..nw {
            let x0 = ox * w / nw;
            let x1 = ((ox + 1) * w).div_ceil(nw).clamp(x0 + 1, w);
            let mut sum = [0u32; 4];
            for y in y0..y1 {
                for x in x0..x1 {
                    let i = (y * w + x) * 4;
                    for c in 0..4 {
                        sum[c] += rgba[i + c] as u32;
                    }
                }
            }
            let n = ((y1 - y0) * (x1 - x0)) as u32;
            for c in sum {
                out.push(((c + n / 2) / n) as u8);
            }
        }
    }
    out
}

/// A sub-rectangle of the frame in image pixels: `x0..x1` by `y0..y1`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CropRect {
    pub x0: u32,
    pub y0: u32,
    pub x1: u32,
    pub y1: u32,
}

impl CropRect {
    pub fn width(&self) -> u32 {
        self.x1 - self.x0
    }
    pub fn height(&self) -> u32 {
        self.y1 - self.y0
    }
}

/// Copy the `crop` region out of a `w`-pixel-wide RGBA buffer.
fn crop_rgba(rgba: &[u8], w: u32, crop: CropRect) -> Vec<u8> {
    let mut out = Vec::with_capacity((crop.width() * crop.height() * 4) as usize);
    for y in crop.y0..crop.y1 {
        let start = ((y * w + crop.x0) * 4) as usize;
        out.extend_from_slice(&rgba[start..start + (crop.width() * 4) as usize]);
    }
    out
}

/// Render `frame` with overlays, optionally crop it to `crop`, then shrink to
/// `percent` of the (cropped) size. Returns the RGBA buffer and its width/height.
fn render_output(
    frame: &Frame,
    params: ToneMapParams,
    overlays: &OverlaySettings,
    crop: Option<CropRect>,
    percent: u32,
) -> (Vec<u8>, u32, u32) {
    let full = CropRect { x0: 0, y0: 0, x1: frame.width, y1: frame.height };
    let crop = crop.unwrap_or(full);
    let mut rgba = render_rgba(frame, params, overlays);
    if crop != full {
        rgba = crop_rgba(&rgba, frame.width, crop);
    }
    let (w, h) = scaled_size(crop.width(), crop.height(), percent);
    let rgba = downscale_area(rgba, crop.width(), crop.height(), w, h);
    (rgba, w, h)
}

/// Render `frame` (see [`render_output`]) as an egui image, e.g. for the clipboard.
pub fn render_color_image(
    frame: &Frame,
    params: ToneMapParams,
    overlays: &OverlaySettings,
    crop: Option<CropRect>,
    percent: u32,
) -> egui::ColorImage {
    let (rgba, w, h) = render_output(frame, params, overlays, crop, percent);
    egui::ColorImage::from_rgba_unmultiplied([w as usize, h as usize], &rgba)
}

/// Render `frame` with overlays (see [`render_output`]), then save as PNG with
/// metadata tEXt chunks. Returns the full path of the written file.
pub fn export_png(
    frame: &Frame,
    params: ToneMapParams,
    overlays: &OverlaySettings,
    save_dir: &Path,
    crop: Option<CropRect>,
    percent: u32,
) -> anyhow::Result<PathBuf> {
    let (rgba, w, h) = render_output(frame, params, overlays, crop, percent);

    // --- PNG metadata text chunks ---
    let meta = &frame.metadata;
    let mut chunks: Vec<(String, String)> = vec![
        ("Software".into(), format!("pumpkin v{}", env!("CARGO_PKG_VERSION"))),
    ];
    macro_rules! chunk {
        ($key:expr, $opt:expr, $fmt:literal) => {
            if let Some(v) = $opt { chunks.push(($key.into(), format!($fmt, v))); }
        };
    }
    // Output pixel = (source pixel - crop origin) * sx, so geometry follows the crop/scale.
    let src = crop.unwrap_or(CropRect { x0: 0, y0: 0, x1: frame.width, y1: frame.height });
    let sx = w as f64 / src.width() as f64;
    let sy = h as f64 / src.height() as f64;
    chunk!("BeamCenterX",      meta.beam_center_x.map(|v| (v - src.x0 as f64) * sx), "{:.4} px");
    chunk!("BeamCenterY",      meta.beam_center_y.map(|v| (v - src.y0 as f64) * sy), "{:.4} px");
    chunk!("DetectorDistance", meta.detector_distance,  "{:.6} m");
    chunk!("Wavelength",       meta.wavelength,         "{:.6} Å");
    chunk!("IncidentEnergy",   meta.incident_energy,    "{:.3} eV");
    chunk!("PixelSizeX",       meta.pixel_size_x.map(|v| v / sx), "{:.3e} m");
    chunk!("PixelSizeY",       meta.pixel_size_y.map(|v| v / sy), "{:.3e} m");
    chunk!("ExposureTime",     meta.exposure_time,      "{:.6} s");
    chunk!("Nimages",          meta.nimages,            "{}");
    chunk!("Ntrigger",         meta.ntrigger,           "{}");
    chunk!("ImageNumber",      meta.image_number,       "{}");
    chunk!("Date",             meta.data_collection_date.clone(),"{}");
    chunk!("SeriesId",         meta.series_id,          "{}");
    if let Some(ref np) = meta.name_pattern {
        chunks.push(("NamePattern".into(), np.clone()));
    }
    if let Some(c) = crop {
        chunks.push((
            "CropRegion".into(),
            format!("x={} y={} w={} h={} (source pixels)", c.x0, c.y0, c.width(), c.height()),
        ));
    }

    // --- Write file ---
    let filename = derive_filename(frame);
    let path = save_dir.join(&filename);
    let file = std::fs::File::create(&path)
        .with_context(|| format!("Cannot create {}", path.display()))?;
    let buf = BufWriter::new(file);

    let mut encoder = png::Encoder::new(buf, w, h);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    for (key, value) in chunks {
        encoder.add_text_chunk(key, value)?;
    }
    let mut writer = encoder.write_header()?;
    writer.write_image_data(&rgba)?;

    Ok(path)
}

// --- raster drawing helpers ---

fn set_pixel(rgba: &mut [u8], width: u32, height: u32, x: i32, y: i32, color: egui::Color32) {
    if x < 0 || y < 0 || x >= width as i32 || y >= height as i32 {
        return;
    }
    let i = (y as u32 * width + x as u32) as usize * 4;
    let a = color.a() as f32 / 255.0;
    let ia = 1.0 - a;
    rgba[i]   = (rgba[i]   as f32 * ia + color.r() as f32 * a).round() as u8;
    rgba[i+1] = (rgba[i+1] as f32 * ia + color.g() as f32 * a).round() as u8;
    rgba[i+2] = (rgba[i+2] as f32 * ia + color.b() as f32 * a).round() as u8;
    rgba[i+3] = 255;
}

fn draw_line(rgba: &mut [u8], width: u32, height: u32,
             x0: f32, y0: f32, x1: f32, y1: f32,
             color: egui::Color32, stroke_width: f32) {
    let len = (x1 - x0).hypot(y1 - y0);
    let steps = (len * 2.0).ceil() as usize + 1;
    let r = (stroke_width / 2.0).ceil() as i32;
    for i in 0..=steps {
        let t = i as f32 / steps as f32;
        let cx = (x0 + t * (x1 - x0)).round() as i32;
        let cy = (y0 + t * (y1 - y0)).round() as i32;
        for dy in -r..=r {
            for dx in -r..=r {
                if dx * dx + dy * dy <= r * r {
                    set_pixel(rgba, width, height, cx + dx, cy + dy, color);
                }
            }
        }
    }
}

fn draw_circle(rgba: &mut [u8], width: u32, height: u32,
               cx: f32, cy: f32, radius: f32,
               color: egui::Color32, stroke_width: f32) {
    let steps = ((2.0 * std::f32::consts::PI * radius) * 2.0).ceil() as usize + 4;
    let r = (stroke_width / 2.0).ceil() as i32;
    for i in 0..steps {
        let angle = 2.0 * std::f32::consts::PI * i as f32 / steps as f32;
        let px = (cx + radius * angle.cos()).round() as i32;
        let py = (cy + radius * angle.sin()).round() as i32;
        for dy in -r..=r {
            for dx in -r..=r {
                if dx * dx + dy * dy <= r * r {
                    set_pixel(rgba, width, height, px + dx, py + dy, color);
                }
            }
        }
    }
}

fn draw_resolution_rings(rgba: &mut [u8], width: u32, height: u32, frame: &Frame, overlays: &OverlaySettings) {
    let Some(geometry) = Geometry::from_metadata(&frame.metadata) else {
        return;
    };
    let (cx, cy) = geometry.beam_center;

    let color = overlays.ring_color;
    let sw = overlays.ring_stroke_width.max(1.0);

    for ring in &overlays.resolution_rings {
        let Some(radius_px) = geometry.ring_radius_px(ring.d_spacing) else { continue };
        draw_circle(rgba, width, height, cx as f32, cy as f32, radius_px as f32, color, sw);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scaled_size_rounds_and_clamps() {
        assert_eq!(scaled_size(100, 50, 50), (50, 25));
        assert_eq!(scaled_size(100, 50, 100), (100, 50));
        assert_eq!(scaled_size(3, 3, 10), (1, 1));
        assert_eq!(scaled_size(100, 50, 0), (1, 1)); // 0 is treated as 1%
        assert_eq!(scaled_size(100, 50, 250), (100, 50));
    }

    #[test]
    fn downscale_averages_blocks() {
        // 2x2 image: black, white / white, black -> 1x1 mid grey
        let px = |v: u8| [v, v, v, 255];
        let mut img = Vec::new();
        for v in [0, 255, 255, 0] {
            img.extend_from_slice(&px(v));
        }
        assert_eq!(downscale_area(img, 2, 2, 1, 1), vec![128, 128, 128, 255]);
    }

    #[test]
    fn crop_extracts_region() {
        // 3x2 image, one distinct byte per pixel in the R channel.
        let mut img = Vec::new();
        for v in 0..6u8 {
            img.extend_from_slice(&[v, 0, 0, 255]);
        }
        let out = crop_rgba(&img, 3, CropRect { x0: 1, y0: 0, x1: 3, y1: 2 });
        let reds: Vec<u8> = out.chunks_exact(4).map(|p| p[0]).collect();
        assert_eq!(reds, vec![1, 2, 4, 5]);
    }

    #[test]
    fn downscale_same_size_is_identity() {
        let img: Vec<u8> = (0..16).collect();
        assert_eq!(downscale_area(img.clone(), 2, 2, 2, 2), img);
    }
}
