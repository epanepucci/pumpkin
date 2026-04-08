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
        Self {
            zoom: 1.0,
            offset: Vec2::ZERO,
        }
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
        self.offset.x = self.offset.x.clamp(
            -vw / self.zoom + margin,
            image_w - margin,
        );
        self.offset.y = self.offset.y.clamp(
            -vh / self.zoom + margin,
            image_h - margin,
        );
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
        let focal = response
            .hover_pos()
            .unwrap_or(response.rect.center());
        let factor = (scroll * 0.002).exp(); // ~0.2% per scroll unit
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
pub fn draw_overlays(
    painter: &Painter,
    view: &ViewState,
    viewport: Rect,
    frame: &Frame,
    overlays: &OverlaySettings,
) {
    let origin = viewport.min;

    // Beam center crosshair.
    if overlays.show_beam_center {
        if let (Some(cx), Some(cy)) = (
            frame.metadata.beam_center_x,
            frame.metadata.beam_center_y,
        ) {
            let center = view.image_to_screen(Pos2::new(cx as f32, cy as f32), origin);
            let arm = 12.0;
            let stroke = egui::Stroke::new(1.5, egui::Color32::YELLOW);
            painter.line_segment(
                [center + Vec2::new(-arm, 0.0), center + Vec2::new(arm, 0.0)],
                stroke,
            );
            painter.line_segment(
                [center + Vec2::new(0.0, -arm), center + Vec2::new(0.0, arm)],
                stroke,
            );
        }
    }

    // Resolution rings.
    if overlays.show_resolution_rings {
        draw_resolution_rings(painter, view, viewport, frame, overlays);
    }

    // Pixel value labels when zoomed in enough.
    if view.zoom >= 8.0 {
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
    let (Some(cx), Some(cy), Some(wavelength), Some(distance), Some(px), Some(py)) = (
        meta.beam_center_x,
        meta.beam_center_y,
        meta.wavelength,
        meta.detector_distance,
        meta.pixel_size_x,
        meta.pixel_size_y,
    ) else {
        return;
    };

    let origin = viewport.min;
    let center_screen = view.image_to_screen(Pos2::new(cx as f32, cy as f32), origin);
    let px_screen_x = view.zoom / px as f32; // screen pixels per metre in x

    let stroke = egui::Stroke::new(1.0, egui::Color32::from_rgba_unmultiplied(0, 200, 255, 180));

    for &d_spacing in &overlays.resolution_rings_angstrom {
        // Bragg: sin(theta) = lambda / (2 * d)
        let sin_theta = wavelength / (2.0 * d_spacing);
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

        // Label the ring with its d-spacing.
        let label_pos = center_screen + Vec2::new(radius_screen * 0.707, -radius_screen * 0.707);
        if viewport.contains(label_pos) {
            painter.text(
                label_pos,
                egui::Align2::LEFT_BOTTOM,
                format!("{:.2} Å", d_spacing),
                egui::FontId::proportional(11.0),
                egui::Color32::from_rgba_unmultiplied(0, 200, 255, 220),
            );
        }

        let _ = (px_screen_x, py);
    }
}

/// Draw pixel values for every visible pixel when zoomed in past the threshold.
fn draw_pixel_values(
    painter: &Painter,
    view: &ViewState,
    viewport: Rect,
    frame: &Frame,
) {
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

    let font = egui::FontId::monospace(view.zoom * 0.4);

    for py in y0..y1 {
        for px in x0..x1 {
            let value = frame.pixels[(py * frame.width + px) as usize];
            let cell_center = view.image_to_screen(
                Pos2::new(px as f32 + 0.5, py as f32 + 0.5),
                origin,
            );
            if !viewport.contains(cell_center) {
                continue;
            }
            let color = if frame.is_saturated(value) {
                egui::Color32::RED
            } else {
                egui::Color32::WHITE
            };
            painter.text(
                cell_center,
                egui::Align2::CENTER_CENTER,
                value.to_string(),
                font.clone(),
                color,
            );
        }
    }
}

/// Which overlays to render.
#[derive(Clone)]
pub struct OverlaySettings {
    pub show_beam_center: bool,
    pub show_resolution_rings: bool,
    /// d-spacings in Angstroms for resolution rings to draw.
    pub resolution_rings_angstrom: Vec<f64>,
}

impl Default for OverlaySettings {
    fn default() -> Self {
        Self {
            show_beam_center: true,
            show_resolution_rings: false,
            resolution_rings_angstrom: vec![10.0, 5.0, 3.0, 2.0, 1.5, 1.0],
        }
    }
}
