use std::io::BufWriter;
use std::path::{Path, PathBuf};

use anyhow::Context;

use crate::frame::Frame;
use crate::image_render::{tone_map, Colormap};
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

/// Tone-map `frame`, draw overlays onto the RGBA buffer, then save as PNG with
/// metadata tEXt chunks.  Returns the full path of the written file.
pub fn export_png(
    frame: &Frame,
    vmin: f32,
    vmax: f32,
    gamma_correction: f32,
    saturation: u16,
    colormap: Colormap,
    overlays: &OverlaySettings,
    save_dir: &Path,
) -> anyhow::Result<PathBuf> {
    let w = frame.width;
    let h = frame.height;

    let mut rgba = tone_map(&frame.pixels, w, h, vmin, vmax, gamma_correction, saturation, colormap);

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
    chunk!("BeamCenterX",      meta.beam_center_x,      "{:.4} px");
    chunk!("BeamCenterY",      meta.beam_center_y,      "{:.4} px");
    chunk!("DetectorDistance", meta.detector_distance,  "{:.6} m");
    chunk!("Wavelength",       meta.wavelength,         "{:.6} Å");
    chunk!("IncidentEnergy",   meta.incident_energy,    "{:.3} eV");
    chunk!("PixelSizeX",       meta.pixel_size_x,       "{:.3e} m");
    chunk!("PixelSizeY",       meta.pixel_size_y,       "{:.3e} m");
    chunk!("FrameTime",        meta.frame_time,         "{:.6} s");
    chunk!("ExposureTime",     meta.exposure_time,      "{:.6} s");
    chunk!("Nimages",          meta.nimages,            "{}");
    chunk!("ImageNumber",      meta.image_number,       "{}");
    chunk!("SeriesId",         meta.series_id,          "{}");
    if let Some(ref np) = meta.name_pattern {
        chunks.push(("NamePattern".into(), np.clone()));
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
    let meta = &frame.metadata;
    let wavelength = meta.wavelength.or_else(|| {
        meta.incident_energy.filter(|&e| e > 0.0).map(|e| 12398.4 / e)
    });
    let (Some(cx), Some(cy), Some(wavelength), Some(distance), Some(px)) = (
        meta.beam_center_x,
        meta.beam_center_y,
        wavelength,
        meta.detector_distance,
        meta.pixel_size_x,
    ) else {
        return;
    };

    let color = overlays.ring_color;
    let sw = overlays.ring_stroke_width.max(1.0);

    for ring in &overlays.resolution_rings {
        let sin_theta = wavelength / (2.0 * ring.d_spacing);
        if sin_theta >= 1.0 { continue; }
        let two_theta = 2.0 * sin_theta.asin();
        let radius_m = distance * two_theta.tan();
        let radius_px = (radius_m / px) as f32;
        draw_circle(rgba, width, height, cx as f32, cy as f32, radius_px, color, sw);
    }
}
