use std::sync::Arc;

use egui::{Painter, Pos2, Rect, Response, Vec2};

use crate::frame::Frame;

/// Zoom/pan state for the image viewport.
#[derive(Clone)]
pub struct ViewState {
    /// Zoom level: screen pixels per image pixel.
    pub zoom: f32,
    /// Top-left image coordinate visible at the viewport origin (in image pixels).
    pub offset: Vec2,
}

impl Default for ViewState {
    fn default() -> Self {
        Self { zoom: 1.0, offset: Vec2::ZERO }
    }
}

impl ViewState {
    /// Convert an image-space position to screen-space within the viewport.
    pub fn image_to_screen(&self, image_pos: Pos2, viewport_origin: Pos2) -> Pos2 {
        Pos2 {
            x: (image_pos.x - self.offset.x) * self.zoom + viewport_origin.x,
            y: (image_pos.y - self.offset.y) * self.zoom + viewport_origin.y,
        }
    }

    /// Convert a screen-space position to image-space.
    pub fn screen_to_image(&self, screen_pos: Pos2, viewport_origin: Pos2) -> Pos2 {
        Pos2 {
            x: (screen_pos.x - viewport_origin.x) / self.zoom + self.offset.x,
            y: (screen_pos.y - viewport_origin.y) / self.zoom + self.offset.y,
        }
    }

    /// Clamp offset so the image cannot be panned entirely off-screen.
    pub fn clamp_offset(&mut self, image_w: f32, image_h: f32, viewport: Rect) {
        let vw = viewport.width();
        let vh = viewport.height();
        let img_screen_w = image_w * self.zoom;
        let img_screen_h = image_h * self.zoom;

        // Allow panning so at least 64 px of the image is always visible.
        let margin = 64.0;
        self.offset.x = self.offset.x.clamp(-vw / self.zoom + margin, image_w - margin);
        self.offset.y = self.offset.y.clamp(-vh / self.zoom + margin, image_h - margin);
        let _ = (img_screen_w, img_screen_h);
    }

    /// Zoom around a focal point (in screen space).
    pub fn zoom_around(&mut self, focal: Pos2, viewport_origin: Pos2, factor: f32) {
        // Image position under the focal point must stay fixed.
        let img_focal = self.screen_to_image(focal, viewport_origin);
        self.zoom = (self.zoom * factor).clamp(0.05, 64.0);
        // Recompute offset so img_focal maps back to focal.
        self.offset.x = img_focal.x - (focal.x - viewport_origin.x) / self.zoom;
        self.offset.y = img_focal.y - (focal.y - viewport_origin.y) / self.zoom;
    }

    /// Fit the whole image into the viewport.
    pub fn fit_to(&mut self, image_w: f32, image_h: f32, viewport: Rect) {
        let zoom_x = viewport.width() / image_w;
        let zoom_y = viewport.height() / image_h;
        self.zoom = zoom_x.min(zoom_y);
        self.offset = Vec2::new(
            -(viewport.width() / self.zoom - image_w) * 0.5,
            -(viewport.height() / self.zoom - image_h) * 0.5,
        );
    }

    /// Zoom to 1:1, centered on the current viewport center.
    pub fn zoom_to_one(&mut self, viewport: Rect) {
        let focal = viewport.center();
        let viewport_origin = viewport.min;
        let img_focal = self.screen_to_image(focal, viewport_origin);
        self.zoom = 1.0;
        self.offset.x = img_focal.x - (focal.x - viewport_origin.x) / self.zoom;
        self.offset.y = img_focal.y - (focal.y - viewport_origin.y) / self.zoom;
    }
}

/// Handle pan and zoom input within the viewport rect. Returns true if view changed.
pub fn handle_input(view: &mut ViewState, response: &Response, frame: Option<&Arc<Frame>>) -> bool {
    let mut changed = false;

    // Pan with left-button drag.
    if response.dragged_by(egui::PointerButton::Primary) {
        let delta = response.drag_delta();
        view.offset.x -= delta.x / view.zoom;
        view.offset.y -= delta.y / view.zoom;
        changed = true;
    }

    // Zoom with scroll wheel, centred on pointer.
    let scroll = response.ctx.input(|i| i.smooth_scroll_delta.y);
    if scroll != 0.0 {
        let focal = response.hover_pos().unwrap_or(response.rect.center());
        let factor = (scroll * 0.02).exp(); // ~0.2% per scroll unit
        view.zoom_around(focal, response.rect.min, factor);
        changed = true;
    }

    if changed {
        if let Some(f) = frame {
            view.clamp_offset(f.width as f32, f.height as f32, response.rect);
        }
    }

    changed
}

/// Draw overlays on the viewport using egui's Painter.
pub fn draw_overlays(painter: &Painter, view: &ViewState, viewport: Rect, frame: &Frame, overlays: &OverlaySettings) {
    let origin = viewport.min;

    // Beam center crosshair.
    if overlays.show_beam_center {
        if let (Some(cx), Some(cy)) = (frame.metadata.beam_center_x, frame.metadata.beam_center_y) {
            let center = view.image_to_screen(Pos2::new(cx as f32, cy as f32), origin);
            let arm = 12.0;
            let stroke = egui::Stroke::new(overlays.beam_center_stroke_width, overlays.beam_center_color);
            painter.line_segment([center + Vec2::new(-arm, 0.0), center + Vec2::new(arm, 0.0)], stroke);
            painter.line_segment([center + Vec2::new(0.0, -arm), center + Vec2::new(0.0, arm)], stroke);
        }
    }

    // Resolution rings.
    if overlays.show_resolution_rings {
        draw_resolution_rings(painter, view, viewport, frame, overlays);
    }

    // Pixel value labels when zoomed in enough.
    if view.zoom >= 15.0 {
        draw_pixel_values(painter, view, viewport, frame);
    }
}

fn draw_resolution_rings(
    painter: &Painter,
    view: &ViewState,
    viewport: Rect,
    frame: &Frame,
    overlays: &OverlaySettings,
) {
    let meta = &frame.metadata;
    // Derive wavelength (Å) from incident_energy (eV) if not directly available:
    // λ = hc/E = 12398.4 eV·Å / E
    let wavelength = meta.wavelength.or_else(|| {
        meta.incident_energy.filter(|&e| e > 0.0).map(|e| 12398.4 / e)
    });
    let (Some(cx), Some(cy), Some(wavelength), Some(distance), Some(px), Some(py)) = (
        meta.beam_center_x,
        meta.beam_center_y,
        wavelength,
        meta.detector_distance,
        meta.pixel_size_x,
        meta.pixel_size_y,
    ) else {
        return;
    };

    let origin = viewport.min;
    let center_screen = view.image_to_screen(Pos2::new(cx as f32, cy as f32), origin);
    let px_screen_x = view.zoom / px as f32; // screen pixels per metre in x

    let stroke = egui::Stroke::new(overlays.ring_stroke_width, overlays.ring_color);
    let font_size = 11.0 * overlays.ring_font_scale;

    for ring in &overlays.resolution_rings {
        // Bragg: sin(theta) = lambda / (2 * d)
        let sin_theta = wavelength / (2.0 * ring.d_spacing);
        if sin_theta >= 1.0 {
            continue;
        }
        let two_theta = 2.0 * sin_theta.asin();
        // Ring radius in metres on detector.
        let radius_m = distance * two_theta.tan();
        // Ring radius in image pixels (using x pixel size; assumes square pixels).
        let radius_px = (radius_m / px) as f32;
        // Ring radius in screen pixels.
        let radius_screen = radius_px * view.zoom;

        painter.circle_stroke(center_screen, radius_screen, stroke);

        let label_pos = center_screen + Vec2::new(radius_screen * 0.707, -radius_screen * 0.707);
        if viewport.contains(label_pos) {
            painter.text(
                label_pos,
                egui::Align2::LEFT_BOTTOM,
                &ring.label,
                egui::FontId::proportional(font_size),
                overlays.ring_color,
            );
        }

        let _ = (px_screen_x, py);
    }
}

/// Draw pixel values for every visible pixel when zoomed in past the threshold.
fn draw_pixel_values(painter: &Painter, view: &ViewState, viewport: Rect, frame: &Frame) {
    let origin = viewport.min;

    // Compute visible image pixel range.
    let top_left = view.screen_to_image(viewport.min, origin);
    let bot_right = view.screen_to_image(viewport.max, origin);

    let x0 = (top_left.x.floor() as i64).max(0) as u32;
    let y0 = (top_left.y.floor() as i64).max(0) as u32;
    let x1 = (bot_right.x.ceil() as i64 + 1).min(frame.width as i64) as u32;
    let y1 = (bot_right.y.ceil() as i64 + 1).min(frame.height as i64) as u32;

    // Limit to avoid overwhelming the painter when zoom is close to threshold.
    let max_labels = 200 * 200;
    if (x1 - x0) as u64 * (y1 - y0) as u64 > max_labels {
        return;
    }

    let font = egui::FontId::proportional(view.zoom * 0.3);

    for py in y0..y1 {
        for px in x0..x1 {
            let value = frame.pixels[(py * frame.width + px) as usize];
            let cell_center = view.image_to_screen(Pos2::new(px as f32 + 0.5, py as f32 + 0.5), origin);
            if !viewport.contains(cell_center) {
                continue;
            }

            if frame.is_saturated(value) {
                continue;
            }

            let color = egui::Color32::GRAY;
            painter.text(cell_center, egui::Align2::CENTER_CENTER, value.to_string(), font.clone(), color);
        }
    }
}

/// A single resolution ring definition.
#[derive(Clone)]
pub struct ResolutionRing {
    /// d-spacing in Ångströms.
    pub d_spacing: f64,
    /// Text drawn next to the ring.
    pub label: String,
}

impl ResolutionRing {
    pub fn new(d_spacing: f64) -> Self {
        Self { d_spacing, label: format!("{d_spacing:.2} Å") }
    }

    pub fn with_label(d_spacing: f64, label: impl Into<String>) -> Self {
        let label = format!("{:.2} Å {}", d_spacing, label.into());
        Self { d_spacing, label: label }
    }
}

/// Which overlays to render and how to style them.
#[derive(Clone)]
pub struct OverlaySettings {
    pub show_beam_center: bool,
    pub beam_center_color: egui::Color32,
    pub beam_center_stroke_width: f32,

    pub show_resolution_rings: bool,
    pub resolution_rings: Vec<ResolutionRing>,
    pub ring_color: egui::Color32,
    pub ring_stroke_width: f32,
    /// Multiplier applied to ring-label font size.
    pub ring_font_scale: f32,
}

impl Default for OverlaySettings {
    fn default() -> Self {
        let cyan = egui::Color32::from_rgba_unmultiplied(0, 200, 255, 200);
        Self {
            show_beam_center: true,
            beam_center_color: cyan,
            beam_center_stroke_width: 1.5,
            show_resolution_rings: true,
            resolution_rings: [10.0, 5.0, 3.0, 2.0, 1.5, 1.0]
                .iter()
                .map(|&d| ResolutionRing::new(d))
                .collect(),
            ring_color: cyan,
            ring_stroke_width: 1.0,
            ring_font_scale: 1.0,
        }
    }
}
