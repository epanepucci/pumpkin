use std::sync::Arc;

use egui::{CentralPanel, Context, Key, KeyboardShortcut, Modifiers, ScrollArea, SidePanel, Ui};
use tokio::sync::watch;

use crate::frame::Frame;
use crate::hdf5_loader::Hdf5Series;
use crate::hdf5_prefetch::HDF5Prefetcher;
use crate::image_render::{Colormap, ImageTexture};
use crate::monitor::{MonitorBatch, MonitorConfig, start_monitor_task};
use crate::monitor_prefetch::MonitorPrefetcher;
use crate::viewport::{self, OverlaySettings, ViewState};

/// Tone-mapping controls.
#[derive(Clone, PartialEq)]
pub struct ContrastState {
    pub vmin: f32,
    pub vmax: f32,
    pub auto: bool,
    pub colormap: Colormap,
    /// Power-law exponent applied after linear normalisation: t' = t^attenuation.
    /// 1.0 = linear (no change); >1.0 darkens background, preserves bright peaks.
    pub attenuation: f32,
}

impl Default for ContrastState {
    fn default() -> Self {
        Self { vmin: 0.0, vmax: 1000.0, auto: true, colormap: Colormap::Inferno, attenuation: 1.0 }
    }
}

pub struct PumpkinApp {
    frame: Option<Arc<Frame>>,
    frame_rx: Option<watch::Receiver<Option<MonitorBatch>>>,

    image_texture: ImageTexture,
    view: ViewState,
    contrast: ContrastState,
    overlays: OverlaySettings,

    /// Trigger a fit-to-view on the next frame.
    pending_fit: bool,

    dcu_url: String,
    poll_period_ms: u64,
    connected: bool,

    /// Pre-fetched monitor frames (browse mode when ≤4, single frame otherwise).
    monitor_frames: Vec<Arc<Frame>>,
    monitor_frame_index: usize,
    monitor_series_id: Option<u64>,
    /// VRAM cache for tone-mapped monitor frames.
    monitor_prefetcher: MonitorPrefetcher,

    /// Open HDF5 series, if any.
    hdf5_series: Option<Hdf5Series>,
    /// Path to the master file (kept for the prefetch thread to open its own handle).
    hdf5_master_path: Option<std::path::PathBuf>,
    /// Current frame index within the HDF5 series.
    hdf5_frame_index: usize,
    /// Background prefetcher: loads + tone-maps neighboring frames into VRAM.
    hdf5_prefetcher: Option<HDF5Prefetcher>,
    /// Contrast params last used to schedule prefetch requests; invalidate on change.
    prefetch_contrast: (f32, f32, f32, Colormap),

    // goto a given frame by number
    show_goto_frame: bool,
    goto_frame_input: String,
}

impl PumpkinApp {
    pub fn new(
        _cc: &eframe::CreationContext,
        dcu_url: String,
        poll_period_ms: u64,
        auto_connect: bool,
        contrast: ContrastState,
        overlays: OverlaySettings,
    ) -> Self {
        let mut app = Self {
            frame: None,
            frame_rx: None,
            image_texture: ImageTexture::default(),
            view: ViewState::default(),
            contrast,
            overlays,
            pending_fit: false,
            dcu_url,
            poll_period_ms,
            connected: false,
            monitor_frames: Vec::new(),
            monitor_frame_index: 0,
            monitor_series_id: None,
            monitor_prefetcher: MonitorPrefetcher::new(),
            hdf5_series: None,
            hdf5_master_path: None,
            hdf5_frame_index: 0,
            hdf5_prefetcher: None,
            prefetch_contrast: (f32::NAN, f32::NAN, f32::NAN, Colormap::Inferno),
            show_goto_frame: false,
            goto_frame_input: "0".to_string(),
        };
        if auto_connect {
            app.connect();
        }
        app
    }

    pub fn load_tiff_file(&mut self, path: &std::path::Path) -> anyhow::Result<()> {
        let data = std::fs::read(path)?;
        let frame = crate::tiff_loader::decode_tiff(&data)?;
        self.on_new_frame(Arc::new(frame));
        Ok(())
    }

    pub fn load_hdf5_master(&mut self, path: &std::path::Path) -> anyhow::Result<()> {
        let series = Hdf5Series::open(path)?;
        let first = series.load_frame(0)?;
        let prefetcher = HDF5Prefetcher::new(path.to_path_buf(), series.saturation_value);
        self.hdf5_master_path = Some(path.to_path_buf());
        self.hdf5_series = Some(series);
        self.hdf5_prefetcher = Some(prefetcher);
        self.hdf5_frame_index = 0;
        self.on_new_frame(Arc::new(first));
        self.schedule_hdf5_prefetch();
        Ok(())
    }

    /// Navigate to an HDF5 frame, using the VRAM cache when available.
    fn load_hdf5_frame(&mut self, index: usize) {
        // Fast path: texture + frame already prefetched into VRAM.
        if let Some(ref prefetcher) = self.hdf5_prefetcher {
            if let Some(cached) = prefetcher.get(index) {
                // Clone the Frame out so we can release the borrow on self.
                let frame = Arc::new(cached.frame.clone());
                self.display_frame(frame);
                self.schedule_hdf5_prefetch();
                return;
            }
        }

        // Slow path: synchronous disk read (prefetcher will cover this on the next navigation).
        if let Some(ref series) = self.hdf5_series {
            match series.load_frame(index) {
                Ok(frame) => self.on_new_frame(Arc::new(frame)),
                Err(e) => eprintln!("HDF5 frame {index}: {e:#}"),
            }
        }
        self.schedule_hdf5_prefetch();
    }

    /// Request the current frame and its neighbors from the background prefetcher.
    fn schedule_hdf5_prefetch(&mut self) {
        let Some(ref series) = self.hdf5_series else { return };
        let Some(ref mut prefetcher) = self.hdf5_prefetcher else { return };
        let total = series.total_frames;
        let cur = self.hdf5_frame_index;
        let vmin = self.contrast.vmin;
        let vmax = self.contrast.vmax;
        let attenuation = self.contrast.attenuation;
        let colormap = self.contrast.colormap;

        for offset in 0..=3usize {
            if cur + offset < total {
                prefetcher.request(cur + offset, vmin, vmax, attenuation, colormap);
            }
            if offset > 0 && cur >= offset {
                prefetcher.request(cur - offset, vmin, vmax, attenuation, colormap);
            }
        }
        prefetcher.evict_distant(cur);
        self.prefetch_contrast = (vmin, vmax, attenuation, colormap);
    }

    pub fn goto_frame(&mut self, ctx: &egui::Context) {
        if !self.show_goto_frame {
            return;
        }

        egui::Window::new("Go to frame")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.label("Frame number:");

                let response = ui.text_edit_singleline(&mut self.goto_frame_input);

                // Auto-focus when opened
                if response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    if let Ok(frame) = self.goto_frame_input.parse::<usize>() {
                        self.do_goto_frame(frame);
                    }
                    self.show_goto_frame = false;
                }

                // ESC cancels
                if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                    self.show_goto_frame = false;
                }

                ui.horizontal(|ui| {
                    if ui.button("Go").clicked() {
                        if let Ok(frame) = self.goto_frame_input.parse::<usize>() {
                            self.do_goto_frame(frame);
                        }
                        self.show_goto_frame = false;
                    }

                    if ui.button("Cancel").clicked() {
                        self.show_goto_frame = false;
                    }
                });
            });
    }

    fn do_goto_frame(&mut self, frame: usize) {
        self.hdf5_frame_index = frame;
        self.load_hdf5_frame(frame);
    }

    /// Update displayed frame and auto-contrast without touching pending_fit.
    fn display_frame(&mut self, frame: Arc<Frame>) {
        if self.contrast.auto {
            let (vmin, vmax) = auto_contrast(&frame);
            self.contrast.vmin = vmin;
            self.contrast.vmax = vmax;
        }
        self.frame = Some(frame);
    }

    fn on_new_frame(&mut self, frame: Arc<Frame>) {
        self.display_frame(frame);
        self.pending_fit = true;
    }

    fn on_monitor_batch(&mut self, batch: MonitorBatch) {
        let new_series = Some(batch.series_id) != self.monitor_series_id;
        self.monitor_series_id = Some(batch.series_id);
        self.monitor_frames = batch.frames;

        if new_series {
            // Start at the most recent frame and fit to view.
            self.monitor_frame_index = self.monitor_frames.len().saturating_sub(1);
            self.pending_fit = true;
        } else {
            // Keep current index, clamped to valid range.
            self.monitor_frame_index = self.monitor_frame_index.min(self.monitor_frames.len().saturating_sub(1));
        }

        // Kick off background tone-mapping for all frames in the batch.
        self.monitor_prefetcher.submit_batch(
            &self.monitor_frames,
            new_series,
            self.contrast.vmin,
            self.contrast.vmax,
            self.contrast.attenuation,
            self.contrast.colormap,
        );

        if let Some(frame) = self.monitor_frames.get(self.monitor_frame_index) {
            self.display_frame(frame.clone());
        }
    }

    fn connect(&mut self) {
        let cfg = MonitorConfig {
            dcu_url: self.dcu_url.clone(),
            api_version: "1.8.0".to_string(),
            poll_period_ms: self.poll_period_ms,
        };
        self.frame_rx = Some(start_monitor_task(cfg));
        self.connected = true;
    }

    fn disconnect(&mut self) {
        self.frame_rx = None;
        self.connected = false;
        self.monitor_frames.clear();
        self.monitor_series_id = None;
        self.monitor_frame_index = 0;
        self.monitor_prefetcher.invalidate();
    }

    fn poll_new_frame(&mut self) -> bool {
        let Some(ref mut rx) = self.frame_rx else {
            return false;
        };
        if !rx.has_changed().unwrap_or(false) {
            return false;
        }
        let maybe_batch = rx.borrow_and_update().clone();
        if let Some(batch) = maybe_batch {
            self.on_monitor_batch(batch);
            return true;
        }
        false
    }

    fn show_left_panel(&mut self, ui: &mut Ui) {
        ui.heading("Connection");
        ui.horizontal(|ui| {
            ui.label("DCU URL:");
            ui.text_edit_singleline(&mut self.dcu_url);
        });
        if self.connected {
            if ui.button("Disconnect").clicked() {
                self.disconnect();
            }
        } else if ui.button("Connect").clicked() {
            self.connect();
        }
        ui.separator();

        ui.heading("Contrast");
        ui.checkbox(&mut self.contrast.auto, "Auto");
        ui.add_enabled(!self.contrast.auto, egui::Slider::new(&mut self.contrast.vmin, 0.0..=65535.0).text("Background"));
        ui.add_enabled(!self.contrast.auto, egui::Slider::new(&mut self.contrast.vmax, 1.0..=65535.0).text("Foreground"));
        ui.add(
            egui::Slider::new(&mut self.contrast.attenuation, 1.0..=10.0)
                .step_by(0.1)
                .text("Gamma"),
        );
        egui::ComboBox::from_label("Colormap")
            .selected_text(self.contrast.colormap.label())
            .show_ui(ui, |ui| {
                for &cmap in Colormap::ALL {
                    ui.selectable_value(&mut self.contrast.colormap, cmap, cmap.label());
                }
            });

        // Colormap preview bar — full panel width, 1 px per sample.
        let bar_height = 16.0;
        let (rect, _) =
            ui.allocate_exact_size(egui::vec2(ui.available_width(), bar_height), egui::Sense::hover());
        if ui.is_rect_visible(rect) {
            let painter = ui.painter();
            let n = rect.width().ceil() as usize;
            for i in 0..n {
                let t = i as f32 / (n.saturating_sub(1)) as f32;
                let [r, g, b] = self.contrast.colormap.apply(t);
                painter.rect_filled(
                    egui::Rect::from_min_max(
                        egui::pos2(rect.left() + i as f32, rect.top()),
                        egui::pos2(rect.left() + i as f32 + 1.0, rect.bottom()),
                    ),
                    0.0,
                    egui::Color32::from_rgb(r, g, b),
                );
            }
        }
        ui.separator();

        ui.heading("Overlays");

        ui.checkbox(&mut self.overlays.show_beam_center, "Beam center");
        egui::Grid::new("beam_center_grid").num_columns(2).show(ui, |ui| {
            ui.label("  Color");
            ui.color_edit_button_srgba(&mut self.overlays.beam_center_color);
            ui.end_row();
            ui.label("  Width");
            ui.add(egui::Slider::new(&mut self.overlays.beam_center_stroke_width, 0.5..=5.0));
            ui.end_row();
        });

        ui.checkbox(&mut self.overlays.show_resolution_rings, "Resolution rings");
        egui::Grid::new("rings_grid").num_columns(2).show(ui, |ui| {
            ui.label("  Color");
            ui.color_edit_button_srgba(&mut self.overlays.ring_color);
            ui.end_row();
            ui.label("  Width");
            ui.add(egui::Slider::new(&mut self.overlays.ring_stroke_width, 0.5..=5.0));
            ui.end_row();
            ui.label("  Font scale");
            ui.add(egui::Slider::new(&mut self.overlays.ring_font_scale, 0.5..=3.0));
            ui.end_row();
        });

        ui.separator();

        ui.heading("Metadata");
        if let Some(ref frame) = self.frame {
            let meta = &frame.metadata;
            egui::Grid::new("meta_grid").num_columns(2).show(ui, |ui| {
                macro_rules! row {
                    ($label:expr, $val:expr) => {
                        ui.label($label);
                        ui.label($val);
                        ui.end_row();
                    };
                }
                row!("Size", format!("{}×{}", frame.width, frame.height));
                row!("Beam X", meta.beam_center_x.map_or("-".into(), |v| format!("{v:.1} px")));
                row!("Beam Y", meta.beam_center_y.map_or("-".into(), |v| format!("{v:.1} px")));
                row!("Distance", meta.detector_distance.map_or("-".into(), |v| format!("{:.1} mm", v * 1000.0)));
                row!("Wavelength", meta.wavelength.map_or("-".into(), |v| format!("{v:.4} Å")));
                row!("Energy", meta.incident_energy.map_or("-".into(), |v| format!("{:.3} keV", v / 1000.0)));
                row!("Frame time", meta.frame_time.map_or("-".into(), |v| format!("{:.1} ms", v * 1000.0)));
                row!("Exposure", meta.exposure_time.map_or("-".into(), |v| format!("{v:.4} s")));
                if let Some(n) = meta.nimages {
                    row!("N images", n.to_string());
                }
                if let Some(n) = meta.image_number {
                    row!("Image #", n.to_string());
                }
                if let Some(ref p) = meta.name_pattern {
                    let name = std::path::Path::new(p)
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or(p.as_str());
                    row!("Name", name.to_string());
                }
            });
        } else {
            ui.label("No image loaded.");
        }
        ui.separator();

        ui.heading("Open file");
        ui.horizontal(|ui| {
            if ui.button("Open TIFF…").clicked() {
                if let Some(path) = rfd::FileDialog::new().add_filter("TIFF", &["tif", "tiff"]).pick_file() {
                    if let Err(e) = self.load_tiff_file(&path) {
                        eprintln!("Failed to load {}: {e}", path.display());
                    }
                }
            }
            if ui.button("Open HDF5…").clicked() {
                if let Some(path) = rfd::FileDialog::new().add_filter("HDF5 master", &["h5"]).pick_file() {
                    if let Err(e) = self.load_hdf5_master(&path) {
                        eprintln!("Failed to open {}: {e}", path.display());
                    }
                }
            }
        });

        // Frame browser — only shown when an HDF5 series is open.
        if let Some(ref series) = self.hdf5_series {
            ui.separator();
            ui.heading("Frame browser");
            let total = series.total_frames;
            ui.label(format!("{total} frames"));

            let old_index = self.hdf5_frame_index;

            // Slider over the full range.
            ui.add(egui::Slider::new(&mut self.hdf5_frame_index, 0..=total.saturating_sub(1)).text("frame"));

            ui.horizontal(|ui| {
                if ui.button("◀").clicked() && self.hdf5_frame_index > 0 {
                    self.hdf5_frame_index -= 1;
                }
                if ui.button("▶").clicked() && self.hdf5_frame_index + 1 < total {
                    self.hdf5_frame_index += 1;
                }
                if ui.button("|◀").clicked() {
                    self.hdf5_frame_index = 0;
                }
                if ui.button("▶|").clicked() {
                    self.hdf5_frame_index = total.saturating_sub(1);
                }
            });

            if self.hdf5_frame_index != old_index {
                self.load_hdf5_frame(self.hdf5_frame_index);
            }
        }

        // Monitor frame browser — shown when the last series has ≤4 images.
        if self.monitor_frames.len() > 1 {
            ui.separator();
            ui.heading("Frame browser");
            let total = self.monitor_frames.len();
            let series_id = self.monitor_series_id.unwrap_or(0);
            ui.label(format!("Series {series_id} — {total} frames"));

            let old_index = self.monitor_frame_index;
            ui.add(egui::Slider::new(&mut self.monitor_frame_index, 0..=total.saturating_sub(1)).text("frame"));

            ui.horizontal(|ui| {
                if ui.button("|◀").clicked() {
                    self.monitor_frame_index = 0;
                }
                if ui.button("◀").clicked() && self.monitor_frame_index > 0 {
                    self.monitor_frame_index -= 1;
                }
                if ui.button("▶").clicked() && self.monitor_frame_index + 1 < total {
                    self.monitor_frame_index += 1;
                }
                if ui.button("▶|").clicked() {
                    self.monitor_frame_index = total.saturating_sub(1);
                }
            });

            if self.monitor_frame_index != old_index {
                let frame = self.monitor_frames[self.monitor_frame_index].clone();
                self.display_frame(frame);
            }
        }
    }

    fn show_viewport(&mut self, ctx: &Context, ui: &mut Ui) {
        let available = ui.available_rect_before_wrap();
        let response = ui.allocate_rect(available, egui::Sense::click_and_drag());

        let Some(ref frame) = self.frame.clone() else {
            ui.painter().text(
                available.center(),
                egui::Align2::CENTER_CENTER,
                "No image — connect to detector or open a TIFF file",
                egui::FontId::proportional(16.0),
                egui::Color32::GRAY,
            );
            return;
        };

        // Fit to view the first time a frame arrives.
        if self.pending_fit && self.hdf5_frame_index == 0 {
            self.view.fit_to(frame.width as f32, frame.height as f32, available);
            self.pending_fit = false;
        }

        // Handle pan + zoom input.
        viewport::handle_input(&mut self.view, &response, Some(frame));

        // Resolve the GPU texture: use a prefetched VRAM handle when available,
        // otherwise tone-map on-demand via ImageTexture.
        let prefetched_id = self.hdf5_prefetcher
            .as_ref()
            .and_then(|p| p.get(self.hdf5_frame_index))
            .map(|c| c.texture.id())
            .or_else(|| {
                self.monitor_prefetcher
                    .get(self.monitor_frame_index)
                    .map(|h| h.id())
            });

        let texture_id = match prefetched_id {
            Some(id) => id,
            None => {
                let frame_ptr = Arc::as_ptr(frame) as usize;
                let Some(t) = self.image_texture.update(
                    ctx,
                    frame,
                    frame_ptr,
                    self.contrast.vmin,
                    self.contrast.vmax,
                    self.contrast.attenuation,
                    self.contrast.colormap,
                ) else {
                    return;
                };
                t.id()
            }
        };

        // Compute where the image should be rendered on screen.
        let image_screen_rect = egui::Rect::from_min_size(
            available.min - egui::Vec2::new(self.view.offset.x * self.view.zoom, self.view.offset.y * self.view.zoom),
            egui::Vec2::new(frame.width as f32 * self.view.zoom, frame.height as f32 * self.view.zoom),
        );

        // Clip the drawn image to the viewport area.
        let painter = ui.painter().with_clip_rect(available);
        painter.image(
            texture_id,
            image_screen_rect,
            egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
            egui::Color32::WHITE,
        );

        // Draw overlays.
        viewport::draw_overlays(&painter, &self.view, available, frame, &self.overlays);

        // Pixel coordinate + value tooltip at cursor.
        if let Some(hover) = response.hover_pos() {
            let img_pos = self.view.screen_to_image(hover, available.min);
            let ix = img_pos.x as i64;
            let iy = img_pos.y as i64;
            if ix >= 0 && iy >= 0 && ix < frame.width as i64 && iy < frame.height as i64 {
                let value = frame.pixels[(iy as u32 * frame.width + ix as u32) as usize];
                painter.text(
                    available.max - egui::Vec2::new(8.0, 8.0),
                    egui::Align2::RIGHT_BOTTOM,
                    format!("({ix}, {iy}) = {value}"),
                    egui::FontId::monospace(12.0),
                    egui::Color32::WHITE,
                );
            }
        }
    }
}

impl eframe::App for PumpkinApp {
    fn update(&mut self, ctx: &Context, _frame: &mut eframe::Frame) {
        let quit_shortcut = KeyboardShortcut::new(Modifiers::COMMAND, Key::Q);
        let next_image_shortcut = KeyboardShortcut::new(Modifiers::NONE, Key::ArrowRight);
        let previous_image_shortcut = KeyboardShortcut::new(Modifiers::NONE, Key::ArrowLeft);
        let goto_shortcut = KeyboardShortcut::new(Modifiers::COMMAND, Key::G);

        if ctx.input_mut(|i| i.consume_shortcut(&goto_shortcut)) {
            self.show_goto_frame = true;
            self.goto_frame_input.clear();
        }

        if self.show_goto_frame {
            self.goto_frame(ctx);
        }

        if ctx.input_mut(|i| i.consume_shortcut(&quit_shortcut)) {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }

        if let Some(ref series) = self.hdf5_series {
            let total = series.total_frames;
            let old_index = self.hdf5_frame_index;

            if ctx.input_mut(|i| i.consume_shortcut(&previous_image_shortcut)) && self.hdf5_frame_index > 0 {
                self.hdf5_frame_index -= 1;
            }

            if ctx.input_mut(|i| i.consume_shortcut(&next_image_shortcut)) && self.hdf5_frame_index + 1 < total {
                self.hdf5_frame_index += 1;
            }

            if self.hdf5_frame_index != old_index {
                self.load_hdf5_frame(self.hdf5_frame_index);
            }
        } else if self.monitor_frames.len() > 1 {
            let total = self.monitor_frames.len();
            let old_index = self.monitor_frame_index;

            if ctx.input_mut(|i| i.consume_shortcut(&previous_image_shortcut)) && self.monitor_frame_index > 0 {
                self.monitor_frame_index -= 1;
            }
            if ctx.input_mut(|i| i.consume_shortcut(&next_image_shortcut)) && self.monitor_frame_index + 1 < total {
                self.monitor_frame_index += 1;
            }

            if self.monitor_frame_index != old_index {
                let frame = self.monitor_frames[self.monitor_frame_index].clone();
                self.display_frame(frame);
            }
        }

        // Poll prefetchers and upload any completed textures.
        if let Some(ref mut prefetcher) = self.hdf5_prefetcher {
            if prefetcher.poll(ctx) {
                ctx.request_repaint();
            }
        }
        if self.monitor_prefetcher.poll(ctx) {
            ctx.request_repaint();
        }

        // Invalidate prefetcher caches when contrast settings change.
        let cur_contrast = (self.contrast.vmin, self.contrast.vmax, self.contrast.attenuation, self.contrast.colormap);
        if cur_contrast != self.prefetch_contrast {
            if self.hdf5_series.is_some() {
                if let Some(ref mut prefetcher) = self.hdf5_prefetcher {
                    prefetcher.invalidate();
                }
                self.schedule_hdf5_prefetch();
            }
            if !self.monitor_frames.is_empty() {
                self.monitor_prefetcher.invalidate();
                self.monitor_prefetcher.submit_batch(
                    &self.monitor_frames,
                    false,
                    cur_contrast.0,
                    cur_contrast.1,
                    cur_contrast.2,
                    cur_contrast.3,
                );
            }
            // Must update after the block; NaN != NaN would re-trigger every frame otherwise.
            self.prefetch_contrast = cur_contrast;
        }

        if self.poll_new_frame() {
            ctx.request_repaint();
        }

        // Keep polling while connected.
        if self.connected {
            ctx.request_repaint_after(std::time::Duration::from_millis(50));
        }

        SidePanel::left("left_panel").resizable(true).default_width(240.0).show(ctx, |ui| {
            ScrollArea::vertical().show(ui, |ui| {
                self.show_left_panel(ui);
            });
        });

        CentralPanel::default().show(ctx, |ui| {
            self.show_viewport(ctx, ui);
        });
    }
}

/// Compute vmin as the 1st percentile and vmax as 10% of the maximum
/// non-saturated pixel value.
fn auto_contrast(frame: &Frame) -> (f32, f32) {
    if frame.pixels.is_empty() {
        return (0.0, 65535.0);
    }
    let mut vals: Vec<u16> = frame.pixels.iter().copied().filter(|&v| v < frame.saturation_value).collect();

    if vals.is_empty() {
        return (0.0, frame.saturation_value as f32);
    }

    vals.sort_unstable();

    let p01 = vals[(vals.len() as f32 * 0.01) as usize];
    let max_val = *vals.last().unwrap() as f32;
    let vmax = (max_val * 0.0010).max(p01 as f32 + 1.0);
    (p01 as f32, vmax)
}
