use std::sync::Arc;

use egui::{CentralPanel, Context, Key, KeyboardShortcut, Modifiers, ScrollArea, SidePanel, Ui};
use tokio::sync::watch;

use crate::frame::Frame;
use crate::hdf5_loader::Hdf5Series;
use crate::image_render::ImageTexture;
use crate::monitor::{MonitorConfig, start_monitor_task};
use crate::viewport::{self, OverlaySettings, ViewState};

/// Tone-mapping controls.
#[derive(Clone)]
pub struct ContrastState {
    pub vmin: f32,
    pub vmax: f32,
    pub auto: bool,
}

impl Default for ContrastState {
    fn default() -> Self {
        Self { vmin: 0.0, vmax: 1000.0, auto: true }
    }
}

pub struct PumpkinApp {
    frame: Option<Arc<Frame>>,
    frame_rx: Option<watch::Receiver<Option<Arc<Frame>>>>,

    image_texture: ImageTexture,
    view: ViewState,
    contrast: ContrastState,
    overlays: OverlaySettings,

    /// Trigger a fit-to-view on the next frame.
    pending_fit: bool,

    dcu_url: String,
    connected: bool,

    /// Open HDF5 series, if any.
    hdf5_series: Option<Hdf5Series>,
    /// Current frame index within the HDF5 series.
    hdf5_frame_index: usize,

    // goto a given frame by number
    show_goto_frame: bool,
    goto_frame_input: String,
}

impl PumpkinApp {
    pub fn new(_cc: &eframe::CreationContext) -> Self {
        Self {
            frame: None,
            frame_rx: None,
            image_texture: ImageTexture::default(),
            view: ViewState::default(),
            contrast: ContrastState::default(),
            overlays: OverlaySettings::default(),
            pending_fit: false,
            dcu_url: "http://localhost".to_string(),
            connected: false,
            hdf5_series: None,
            hdf5_frame_index: 0,
            show_goto_frame: false,
            goto_frame_input: "0".to_string(),
        }
    }

    pub fn load_tiff_file(&mut self, path: &std::path::Path) -> anyhow::Result<()> {
        let data = std::fs::read(path)?;
        let frame = crate::tiff_loader::decode_tiff(&data)?;
        self.on_new_frame(Arc::new(frame));
        Ok(())
    }

    pub fn load_hdf5_master(&mut self, path: &std::path::Path) -> anyhow::Result<()> {
        let series = Hdf5Series::open(path)?;
        // Load the first frame immediately so the viewer shows something.
        let first = series.load_frame(0)?;
        self.hdf5_series = Some(series);
        self.hdf5_frame_index = 0;
        self.on_new_frame(Arc::new(first));
        Ok(())
    }

    fn load_hdf5_frame(&mut self, index: usize) {
        if let Some(ref series) = self.hdf5_series {
            match series.load_frame(index) {
                Ok(frame) => self.on_new_frame(Arc::new(frame)),
                Err(e) => eprintln!("HDF5 frame {index}: {e}"),
            }
        }
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
        // 👉 your logic here
        self.hdf5_frame_index = frame;
        self.load_hdf5_frame(self.hdf5_frame_index);
    }

    fn on_new_frame(&mut self, frame: Arc<Frame>) {
        if self.contrast.auto {
            let (vmin, vmax) = auto_contrast(&frame);
            self.contrast.vmin = vmin;
            self.contrast.vmax = vmax;
        }
        self.frame = Some(frame);
        self.pending_fit = true;
    }

    fn connect(&mut self) {
        let cfg =
            MonitorConfig { dcu_url: self.dcu_url.clone(), api_version: "1.8.0".to_string(), poll_timeout_ms: 500 };
        self.frame_rx = Some(start_monitor_task(cfg));
        self.connected = true;
    }

    fn disconnect(&mut self) {
        self.frame_rx = None;
        self.connected = false;
    }

    fn poll_new_frame(&mut self) -> bool {
        let Some(ref mut rx) = self.frame_rx else {
            return false;
        };
        if !rx.has_changed().unwrap_or(false) {
            return false;
        }
        // Clone the Arc out of the watch ref before calling on_new_frame
        // to satisfy the borrow checker.
        let maybe_frame = rx.borrow_and_update().clone();
        if let Some(frame) = maybe_frame {
            self.on_new_frame(frame);
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
        ui.add_enabled(!self.contrast.auto, egui::Slider::new(&mut self.contrast.vmin, 0.0..=65535.0).text("vmin"));
        ui.add_enabled(!self.contrast.auto, egui::Slider::new(&mut self.contrast.vmax, 1.0..=65535.0).text("vmax"));
        ui.separator();

        ui.heading("Overlays");
        ui.checkbox(&mut self.overlays.show_beam_center, "Beam center");
        ui.checkbox(&mut self.overlays.show_resolution_rings, "Resolution rings");
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
                row!("Exposure", meta.exposure_time.map_or("-".into(), |v| format!("{v:.4} s")));
                if let Some(n) = meta.image_number {
                    row!("Image #", n.to_string());
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
        if self.pending_fit {
            self.view.fit_to(frame.width as f32, frame.height as f32, available);
            self.pending_fit = false;
        }

        // Handle pan + zoom input.
        viewport::handle_input(&mut self.view, &response, Some(frame));

        // Get/update the GPU texture.
        let frame_ptr = Arc::as_ptr(frame) as usize;
        let Some(texture) = self.image_texture.update(ctx, frame, frame_ptr, self.contrast.vmin, self.contrast.vmax)
        else {
            return;
        };

        // Compute where the image should be rendered on screen.
        let image_screen_rect = egui::Rect::from_min_size(
            available.min - egui::Vec2::new(self.view.offset.x * self.view.zoom, self.view.offset.y * self.view.zoom),
            egui::Vec2::new(frame.width as f32 * self.view.zoom, frame.height as f32 * self.view.zoom),
        );

        // Clip the drawn image to the viewport area.
        let painter = ui.painter().with_clip_rect(available);
        painter.image(
            texture.id(),
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

/// Compute vmin/vmax as the 1st and 99th percentile of non-saturated pixels.
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
    let p99 = vals[((vals.len() as f32 * 0.99) as usize).min(vals.len() - 1)];
    (p01 as f32, (p99 as f32).max(p01 as f32 + 1.0))
}
