use std::sync::Arc;

use egui::{CentralPanel, Context, Key, KeyboardShortcut, Modifiers, ScrollArea, SidePanel, Ui};
use egui_file_dialog::FileDialog;
use tokio::sync::watch;

use crate::frame::{Frame, FrameMetadata};
use crate::hdf5_loader::Hdf5Series;
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

pub struct PumpkinApp {
    frame: Option<Arc<Frame>>,
    frame_rx: Option<watch::Receiver<Option<MonitorBatch>>>,

    image_texture: ImageTexture,
    /// Incremented every time `self.frame` changes; used as the cache key for
    /// `ImageTexture::update` instead of the raw Arc pointer (which the allocator
    /// can reuse across frames of the same size, causing false cache hits).
    frame_generation: u64,
    view: ViewState,
    contrast: ContrastState,
    overlays: OverlaySettings,

    /// Trigger a fit-to-view on the next frame.
    pending_fit: bool,
    /// Last seen viewport rect, used for zoom buttons in the side panel.
    last_viewport_rect: egui::Rect,

    dcu_url: String,
    poll_period_ms: u64,
    /// Poll period used when the window is visible but not focused (ms).
    unfocused_poll_period_ms: u64,
    /// Pause live frame updates for this duration after zoom/pan. 0 = disabled.
    monitor_pause_ms: u64,
    connected: bool,
    /// Sender side of the monitor poll-period control channel.
    /// Send 0 to pause, any other value to set the interval in ms.
    monitor_ctl_tx: Option<tokio::sync::watch::Sender<u64>>,
    /// Last time the user interacted with the viewport (zoom or pan).
    last_interaction_time: Option<std::time::Instant>,
    /// Whether the monitor was paused on the previous update; used to detect
    /// the pause-expiry transition and resume display immediately.
    was_paused: bool,
    /// Last time any meaningful user input was detected (key, click, scroll).
    /// Used for idle-based monitoring pause.
    last_activity_time: std::time::Instant,
    /// Pause monitoring after this many seconds of inactivity. 0 = disabled.
    idle_pause_secs: u64,

    /// Latest frame from the monitor (single entry, replaced on every poll).
    monitor_frames: Vec<Arc<Frame>>,
    monitor_frame_index: usize,
    monitor_series_id: Option<u64>,
    /// All image IDs available in the current series buffer.
    monitor_image_ids: Vec<u64>,
    /// Currently selected image ID in the frame browser pulldown.
    monitor_selected_id: Option<u64>,
    /// Detector metadata for the current series (used when fetching on-demand frames).
    monitor_series_meta: FrameMetadata,
    /// Channel for on-demand frame fetches triggered from the frame browser.
    on_demand_tx: std::sync::mpsc::SyncSender<Frame>,
    on_demand_rx: std::sync::mpsc::Receiver<Frame>,
    /// VRAM cache for tone-mapped monitor frames.
    monitor_prefetcher: MonitorPrefetcher,
    /// Contrast params last used to submit monitor prefetch requests; invalidate on change.
    monitor_contrast: (f32, f32, f32, Colormap),
    /// True while the user is viewing a specific on-demand frame from the browser;
    /// suppresses prefetcher use and batch re-submission so the cached monitor
    /// texture never overwrites what the user explicitly selected.
    on_demand_active: bool,

    /// Open HDF5 series, if any.
    hdf5_series: Option<Hdf5Series>,
    /// Path to the master file (kept for the prefetch thread to open its own handle).
    hdf5_master_path: Option<std::path::PathBuf>,
    /// Current frame index within the HDF5 series (always a multiple of hdf5_grouping).
    hdf5_frame_index: usize,
    /// Number of consecutive frames to sum before displaying.
    hdf5_grouping: usize,

    saturation_override_enabled: bool,
    saturation_override_value: u16,
    /// Original saturation_value from the file/detector, before any override.
    file_saturation_value: Option<u16>,

    /// Cached histogram counts. Invalidated when frame, saturation, or bin count changes.
    histogram_cache: Option<(usize, u16, usize, Vec<u32>)>, // (frame_ptr, saturation, n_bins, counts)

    // goto a given frame by number
    show_goto_frame: bool,
    goto_frame_input: String,

    auto_region: bool,
    lock_zoom: bool,
    zoom_speed: f32,
    show_help: bool,
    show_panel: bool,
    show_actions: bool,

    start_time: std::time::Instant,

    splash_folder: Option<std::path::PathBuf>,
    splash_texture: Option<egui::TextureHandle>,
    splash_loaded: bool,

    /// Last folder used to open a file.
    last_location: Option<std::path::PathBuf>,

    file_dialog: FileDialog,
}

impl PumpkinApp {
    fn last_location_path() -> Option<std::path::PathBuf> {
        std::env::var_os("HOME").map(|h| {
            std::path::PathBuf::from(h)
                .join(".config")
                .join("pumpkin")
                .join("last_location.txt")
        })
    }

    fn load_last_location() -> Option<std::path::PathBuf> {
        let path = Self::last_location_path()?;
        if path.exists() {
            let s = std::fs::read_to_string(&path).ok()?;
            let p = std::path::PathBuf::from(s.trim());
            if p.exists() {
                return Some(p);
            }
        }
        None
    }

    fn save_last_location(&self) {
        let Some(ref loc) = self.last_location else { return };
        let Some(path) = Self::last_location_path() else { return };

        // Ensure parent directory exists.
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }

        let _ = std::fs::write(&path, loc.to_string_lossy().as_bytes());
    }

    pub fn new(
        _cc: &eframe::CreationContext,
        dcu_url: String,
        poll_period_ms: u64,
        unfocused_poll_period_ms: u64,
        monitor_pause_ms: u64,
        idle_pause_secs: u64,
        auto_connect: bool,
        contrast: ContrastState,
        overlays: OverlaySettings,
        splash_folder: Option<std::path::PathBuf>,
    ) -> Self {
        let (on_demand_tx, on_demand_rx) = std::sync::mpsc::sync_channel(4);
        let mut app = Self {
            frame: None,
            frame_rx: None,
            image_texture: ImageTexture::default(),
            frame_generation: 0,
            view: ViewState::default(),
            contrast,
            overlays,
            pending_fit: false,
            last_viewport_rect: egui::Rect::NOTHING,
            dcu_url,
            poll_period_ms,
            unfocused_poll_period_ms,
            monitor_pause_ms,
            connected: false,
            monitor_ctl_tx: None,
            last_interaction_time: None,
            was_paused: false,
            last_activity_time: std::time::Instant::now(),
            idle_pause_secs,
            monitor_frames: Vec::new(),
            monitor_frame_index: 0,
            monitor_series_id: None,
            monitor_image_ids: Vec::new(),
            monitor_selected_id: None,
            monitor_series_meta: FrameMetadata::default(),
            on_demand_tx,
            on_demand_rx,
            monitor_prefetcher: MonitorPrefetcher::new(),
            monitor_contrast: (f32::NAN, f32::NAN, f32::NAN, Colormap::Inferno),
            on_demand_active: false,
            hdf5_series: None,
            hdf5_master_path: None,
            hdf5_frame_index: 0,
            hdf5_grouping: 1,
            saturation_override_enabled: true,
            saturation_override_value: 32767,
            file_saturation_value: None,
            histogram_cache: None,
            show_goto_frame: false,
            goto_frame_input: "0".to_string(),
            auto_region: false,
            lock_zoom: true,
            zoom_speed: 0.02,
            show_help: false,
            show_panel: true,
            show_actions: false,
            start_time: std::time::Instant::now(),
            splash_folder,
            splash_texture: None,
            splash_loaded: false,
            last_location: Self::load_last_location(),
            file_dialog: FileDialog::new()
                .add_file_filter_extensions("HDF5 master", vec!["h5"]),
        };
        if auto_connect {
            app.connect();
        }
        app
    }

    fn format_uptime(elapsed: std::time::Duration) -> String {
        let secs = elapsed.as_secs();
        if secs < 60 {
            format!("{secs}s")
        } else if secs < 3600 {
            format!("{}m", secs / 60)
        } else if secs < 86400 {
            let h = secs / 3600;
            let m = (secs % 3600) / 60;
            format!("{h}h {m}m")
        } else if secs < 7 * 86400 {
            let d = secs / 86400;
            let h = (secs % 86400) / 3600;
            format!("{d}d {h}h")
        } else {
            let w = secs / (7 * 86400);
            let d = (secs % (7 * 86400)) / 86400;
            format!("{w}w {d}d")
        }
    }

    fn pick_random_png(dir: &std::path::Path) -> Option<std::path::PathBuf> {
        let mut pngs: Vec<std::path::PathBuf> = std::fs::read_dir(dir)
            .ok()?
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| {
                p.extension()
                    .and_then(|e| e.to_str())
                    .map(|e| e.eq_ignore_ascii_case("png"))
                    .unwrap_or(false)
            })
            .collect();
        if pngs.is_empty() {
            return None;
        }
        pngs.sort();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        Some(pngs[nanos as usize % pngs.len()].clone())
    }

    fn decode_splash_png(bytes: &[u8]) -> egui::ColorImage {
        let decoder = png::Decoder::new(std::io::Cursor::new(bytes));
        let mut reader = decoder.read_info().expect("splash PNG info");
        let mut buf = vec![0u8; reader.output_buffer_size().expect("splash PNG buffer size")];
        let info = reader.next_frame(&mut buf).expect("splash PNG frame");
        let raw = &buf[..info.buffer_size()];
        let width = info.width as usize;
        let height = info.height as usize;
        let pixels: Vec<egui::Color32> = match info.color_type {
            png::ColorType::Rgb => raw
                .chunks_exact(3)
                .map(|c| egui::Color32::from_rgb(c[0], c[1], c[2]))
                .collect(),
            png::ColorType::Rgba => raw
                .chunks_exact(4)
                .map(|c| egui::Color32::from_rgba_unmultiplied(c[0], c[1], c[2], c[3]))
                .collect(),
            png::ColorType::Grayscale => raw
                .iter()
                .map(|&v| egui::Color32::from_rgb(v, v, v))
                .collect(),
            png::ColorType::GrayscaleAlpha => raw
                .chunks_exact(2)
                .map(|c| egui::Color32::from_rgba_unmultiplied(c[0], c[0], c[0], c[1]))
                .collect(),
            t => panic!("unsupported splash PNG color type: {t:?}"),
        };
        egui::ColorImage::new([width, height], pixels)
    }

    pub fn load_hdf5_master(&mut self, path: &std::path::Path) -> anyhow::Result<()> {
        let series = Hdf5Series::open(path)?;
        let first = series.load_frame(0)?;
        self.hdf5_master_path = Some(path.to_path_buf());
        self.hdf5_series = Some(series);
        self.hdf5_frame_index = 0;
        self.on_new_frame(Arc::new(first));
        Ok(())
    }

    /// Navigate to an HDF5 frame.
    fn load_hdf5_frame(&mut self, index: usize) {
        if let Some(ref series) = self.hdf5_series {
            match series.load_frame(index) {
                Ok(frame) => self.on_new_frame(Arc::new(frame)),
                Err(e) => eprintln!("HDF5 frame {index}: {e:#}"),
            }
        }
    }

    /// Load and display the group starting at `start_index`, summing `hdf5_grouping` frames.
    /// Falls back to the single-frame path (with prefetcher) when grouping == 1.
    fn load_hdf5_grouped(&mut self, start_index: usize) {
        if self.hdf5_grouping <= 1 {
            self.load_hdf5_frame(start_index);
            return;
        }

        let grouped = {
            let Some(ref series) = self.hdf5_series else { return };
            let end = (start_index + self.hdf5_grouping).min(series.total_frames);
            let mut acc: Option<Vec<u32>> = None;
            let mut width = 0u32;
            let mut height = 0u32;
            let mut metadata = series.series_metadata.clone();

            for idx in start_index..end {
                match series.load_frame(idx) {
                    Ok(frame) => {
                        width = frame.width;
                        height = frame.height;
                        if idx == start_index {
                            metadata = frame.metadata.clone();
                        }
                        match acc {
                            None => acc = Some(frame.pixels.iter().map(|&v| v as u32).collect()),
                            Some(ref mut a) => {
                                for (s, &v) in a.iter_mut().zip(frame.pixels.iter()) {
                                    *s += v as u32;
                                }
                            }
                        }
                    }
                    Err(e) => eprintln!("HDF5 grouped frame {idx}: {e:#}"),
                }
            }
            acc.map(|a| (a, width, height, metadata))
        };

        if let Some((acc, width, height, metadata)) = grouped {
            let effective_sat = self.effective_saturation();
            let pixels: Vec<u16> = acc.iter().map(|&s| s.min(effective_sat as u32) as u16).collect();
            let frame = Frame { pixels, width, height, saturation_value: effective_sat, metadata };
            self.on_new_frame(Arc::new(frame));
        }
        // Don't prefetch individual frames in grouped mode — they would overwrite
        // the grouped texture in the cache and cause the renderer to display the
        // wrong (non-summed) colorization.
    }

    fn effective_saturation(&self) -> u16 {
        if self.saturation_override_enabled {
            self.saturation_override_value
        } else {
            self.file_saturation_value.unwrap_or(u16::MAX)
        }
    }

    /// True if the monitor live display should be paused because the user
    /// interacted with the viewport recently (within `monitor_pause_ms`).
    fn interaction_paused(&self) -> bool {
        if self.monitor_pause_ms == 0 {
            return false;
        }
        self.last_interaction_time
            .map(|t| t.elapsed().as_millis() < self.monitor_pause_ms as u128)
            .unwrap_or(false)
    }

    /// True if monitoring should be paused due to prolonged inactivity.
    fn is_idle_paused(&self) -> bool {
        if self.idle_pause_secs == 0 {
            return false;
        }
        self.last_activity_time.elapsed().as_secs() >= self.idle_pause_secs
    }

    pub fn goto_frame(&mut self, ctx: &egui::Context) {
        if !self.show_goto_frame {
            return;
        }

        egui::Window::new("Go to frame")
            .collapsible(false)
            .resizable(false)
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
        let grouping = self.hdf5_grouping.max(1);
        let snapped = (frame / grouping) * grouping;
        self.hdf5_frame_index = snapped;
        self.load_hdf5_grouped(snapped);
    }

    pub fn open_hdf5_dialog(&mut self) {
        if let Some(ref loc) = self.last_location {
            self.file_dialog.config_mut().initial_directory = loc.clone();
        }
        self.file_dialog.pick_file();
    }

    pub fn show_help_window(&mut self, ctx: &Context) {
        if !self.show_help {
            return;
        }

        egui::Window::new("Help")
            .collapsible(false)
            .resizable(false)
            .show(ctx, |ui| {
                ui.heading("Keyboard Shortcuts");
                egui::Grid::new("help_grid").num_columns(2).spacing([20.0, 8.0]).show(ui, |ui| {
                    ui.label("Ctrl-0"); ui.label("Fit image to view"); ui.end_row();
                    ui.label("Ctrl-1"); ui.label("Zoom to 1:1"); ui.end_row();
                    ui.label("Left / Right"); ui.label("Previous / Next frame"); ui.end_row();
                    ui.label("Ctrl+O"); ui.label("Open HDF5 master"); ui.end_row();
                    ui.label("Ctrl+G"); ui.label("Go to frame number"); ui.end_row();
                    ui.label("Ctrl+Q"); ui.label("Quit"); ui.end_row();
                    ui.label("Tab"); ui.label("Hide / show side panel"); ui.end_row();
                    ui.label("F11"); ui.label("Toggle fullscreen"); ui.end_row();
                    ui.label("?"); ui.label("Show this help"); ui.end_row();
                });

                ui.add_space(10.0);
                ui.separator();
                ui.label(format!("Pumpkin v{}", env!("APP_VERSION")));
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    if ui.button("Close").clicked() || ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                        self.show_help = false;
                    }
                });
            });
    }

    fn show_actions_window(&mut self, ctx: &Context) {
        if !self.show_actions {
            return;
        }

        let mut open = true;
        let mut close_clicked = false;
        egui::Window::new("Actions")
            .collapsible(false)
            .resizable(false)
            .open(&mut open)
            .show(ctx, |ui| {
                ui.heading("Open file");
                ui.horizontal(|ui| {
                    if ui.button("Open HDF5…").on_hover_text("Open HDF5 master file (Ctrl+O)").clicked() {
                        close_clicked = true;
                        self.open_hdf5_dialog();
                    }
                    let save_enabled = self.frame.is_some();
                    if ui.add_enabled(save_enabled, egui::Button::new("Save PNG"))
                        .on_hover_text("Save current image with overlays as PNG")
                        .clicked()
                    {
                        close_clicked = true;
                        self.save_png();
                    }
                });

                ui.add_space(8.0);
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

                ui.add_space(8.0);
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

                ui.add_space(8.0);
                ui.heading("Viewport");
                egui::Grid::new("viewport_grid").num_columns(2).show(ui, |ui| {
                    ui.label("Zoom speed");
                    ui.add(
                        egui::Slider::new(&mut self.zoom_speed, 0.005..=0.1)
                            .step_by(0.005)
                            .fixed_decimals(3),
                    );
                    ui.end_row();
                });

                ui.add_space(8.0);
                if ui.button("Close").clicked() || ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                    close_clicked = true;
                }
            });
        self.show_actions = open && !close_clicked;
    }

    /// Update displayed frame and auto-contrast without touching pending_fit.
    fn display_frame(&mut self, frame: Arc<Frame>) {
        // Record the raw file saturation before any user override.
        self.file_saturation_value = Some(frame.saturation_value);
        let sat = self.effective_saturation();
        let frame = if frame.saturation_value != sat {
            let mut f = (*frame).clone();
            f.saturation_value = sat;
            Arc::new(f)
        } else {
            frame
        };
        if self.contrast.auto {
            let (vmin, vmax) = auto_contrast(&frame);
            self.contrast.vmin = vmin;
            self.contrast.vmax = vmax;
        }
        self.frame = Some(frame);
        self.frame_generation += 1;
    }

    fn on_new_frame(&mut self, frame: Arc<Frame>) {
        self.display_frame(frame);
        self.pending_fit = true;
    }

    fn on_monitor_batch(&mut self, batch: MonitorBatch) {
        let new_series = Some(batch.series_id) != self.monitor_series_id;
        self.monitor_series_id = Some(batch.series_id);
        self.monitor_image_ids = batch.image_ids;
        self.monitor_series_meta = batch.metadata;
        self.monitor_frames = batch.frames;

        if new_series {
            // New series: always go live, discard any on-demand selection.
            self.on_demand_active = false;
            self.monitor_selected_id = self.monitor_image_ids.last().copied();
            self.monitor_frame_index = self.monitor_frames.len().saturating_sub(1);
            if !self.lock_zoom {
                self.pending_fit = true;
            }
        } else {
            // Keep current index, clamped to valid range.
            self.monitor_frame_index = self.monitor_frame_index.min(self.monitor_frames.len().saturating_sub(1));
            // Don't reset the user's pulldown selection while they're viewing a specific frame.
            if !self.on_demand_active {
                self.monitor_selected_id = self.monitor_image_ids.last().copied();
            }
        }

        if self.on_demand_active {
            // User is viewing a specific on-demand frame — don't touch the display or
            // submit prefetch tasks that would later overwrite it via the texture cache.
            return;
        }

        // Invalidate stale textures — frames at the same index change on every poll,
        // so cached textures are never reusable across batches.
        self.monitor_prefetcher.invalidate();

        if self.interaction_paused() {
            // User is actively panning/zooming — keep showing the current frame until
            // the interaction has been idle for monitor_pause_ms.
            return;
        }

        // Kick off background tone-mapping for all frames in the batch.
        self.monitor_prefetcher.submit_batch(
            &self.monitor_frames,
            false, // cache already cleared by invalidate() above
            self.contrast.vmin,
            self.contrast.vmax,
            self.contrast.gamma_correction,
            self.effective_saturation(),
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
        };
        let (period_tx, period_rx) = tokio::sync::watch::channel(self.poll_period_ms);
        self.frame_rx = Some(start_monitor_task(cfg, period_rx));
        self.monitor_ctl_tx = Some(period_tx);
        self.connected = true;
    }

    fn disconnect(&mut self) {
        self.frame_rx = None;
        self.monitor_ctl_tx = None;
        self.connected = false;
        self.monitor_frames.clear();
        self.monitor_series_id = None;
        self.monitor_frame_index = 0;
        self.on_demand_active = false;
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

    fn save_png(&self) {
        let Some(ref frame) = self.frame else { return };
        let save_dir = std::env::var_os("HOME")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| std::path::PathBuf::from("."));
        match crate::png_export::export_png(
            frame,
            self.contrast.vmin,
            self.contrast.vmax,
            self.contrast.gamma_correction,
            self.effective_saturation(),
            self.contrast.colormap,
            &self.overlays,
            &save_dir,
        ) {
            Ok(path) => eprintln!("Saved PNG: {}", path.display()),
            Err(e)   => eprintln!("PNG export failed: {e:#}"),
        }
    }

    fn fetch_monitor_frame_on_demand(&self, image_id: u64) {
        let Some(series_id) = self.monitor_series_id else {
            eprintln!("On-demand: no active series, ignoring request for image {image_id}");
            return;
        };
        let url = format!("{}/monitor/api/1.8.0/images/{}/{}", self.dcu_url, series_id, image_id);
        eprintln!("On-demand: spawning fetch for {series_id}/{image_id} via {url}");
        let meta = self.monitor_series_meta.clone();
        let tx = self.on_demand_tx.clone();
        tokio::spawn(async move {
            let client = reqwest::Client::new();
            eprintln!("On-demand: GET {url}");
            match crate::monitor::fetch_tiff(&client, &url).await {
                Ok(mut frame) => {
                    eprintln!("On-demand: fetch OK for {series_id}/{image_id}, sending to main thread");
                    frame.metadata = meta;
                    frame.metadata.image_number = Some(image_id as i64);
                    if tx.try_send(frame).is_err() {
                        eprintln!("On-demand: channel full or disconnected, frame {series_id}/{image_id} dropped");
                    }
                }
                Err(e) => eprintln!("On-demand: fetch FAILED {series_id}/{image_id}: {e}"),
            }
        });
    }

    fn show_left_panel(&mut self, ui: &mut Ui) {
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
        // HDF5 Frame browser
        if let Some(ref series) = self.hdf5_series {
            ui.heading("HDF5 frame browser");
            let total = series.total_frames;
            let grouping = self.hdf5_grouping.max(1);
            let n_groups = (total / grouping).max(1);

            ui.horizontal(|ui| {
                ui.label(format!("{total} frames"));
                ui.separator();
                ui.label("Grouping:");
                let old_grouping = self.hdf5_grouping;
                ui.add(egui::DragValue::new(&mut self.hdf5_grouping).range(1..=total).speed(0.1));
                if self.hdf5_grouping != old_grouping {
                    // Snap frame index to the new group boundary.
                    let g = self.hdf5_grouping.max(1);
                    self.hdf5_frame_index = (self.hdf5_frame_index / g) * g;
                }
            });

            let old_index = self.hdf5_frame_index;

            let mut group_idx = self.hdf5_frame_index / grouping;
            if ui.add(egui::Slider::new(&mut group_idx, 0..=n_groups.saturating_sub(1)).text("group")).changed() {
                self.hdf5_frame_index = group_idx * grouping;
            }

            ui.horizontal(|ui| {
                if ui.button("◀").clicked() && self.hdf5_frame_index >= grouping {
                    self.hdf5_frame_index -= grouping;
                }
                if ui.button("▶").clicked() && self.hdf5_frame_index + grouping < total {
                    self.hdf5_frame_index += grouping;
                }
                if ui.button("|◀").clicked() {
                    self.hdf5_frame_index = 0;
                }
                if ui.button("▶|").clicked() {
                    self.hdf5_frame_index = (n_groups - 1) * grouping;
                }
            });

            if self.hdf5_frame_index != old_index || self.hdf5_grouping != grouping {
                self.load_hdf5_grouped(self.hdf5_frame_index);
            }
        }

        // Monitor frame browser.
        else if !self.monitor_image_ids.is_empty() {
            let series_id = self.monitor_series_id.unwrap_or(0);
            ui.horizontal(|ui| {
                ui.heading("Monitor browser");
                if self.is_idle_paused() {
                    ui.label(egui::RichText::new("⏸ idle").color(egui::Color32::GRAY).small());
                } else if self.interaction_paused() {
                    ui.label(egui::RichText::new("⏸ paused").color(egui::Color32::YELLOW).small());
                } else if self.on_demand_active {
                    ui.label(egui::RichText::new("browsing").color(egui::Color32::from_rgb(100, 180, 255)).small());
                }
            });
            ui.label(format!("Series {series_id} — {} images", self.monitor_image_ids.len()));

            let prev_selected = self.monitor_selected_id;

            // Find the position of the currently selected ID in the list.
            let cur_pos = self.monitor_selected_id
                .and_then(|id| self.monitor_image_ids.iter().position(|&x| x == id));

            ui.horizontal(|ui| {
                let can_prev = cur_pos.map_or(false, |p| p > 0);
                let can_next = cur_pos.map_or(false, |p| p + 1 < self.monitor_image_ids.len());
                if ui.add_enabled(can_prev, egui::Button::new(egui::RichText::new("◀").size(36.0)).min_size(egui::vec2(80.0, 60.0)).corner_radius(10)).clicked() {
                    self.monitor_selected_id = Some(self.monitor_image_ids[cur_pos.unwrap() - 1]);
                }
                if ui.add_enabled(can_next, egui::Button::new(egui::RichText::new("▶").size(36.0)).min_size(egui::vec2(80.0, 60.0)).corner_radius(10)).clicked() {
                    self.monitor_selected_id = Some(self.monitor_image_ids[cur_pos.unwrap() + 1]);
                }
            });
            ui.horizontal(|ui| {
                ui.label("Go to frame: ");
                egui::ComboBox::from_label("")
                    .selected_text(
                        self.monitor_selected_id.map_or("—".to_string(), |id| id.to_string()),
                    )
                    .show_ui(ui, |ui| {
                        for &id in self.monitor_image_ids.iter().rev() {
                            ui.selectable_value(&mut self.monitor_selected_id, Some(id), id.to_string());
                        }
                    });
            });

            if self.monitor_selected_id != prev_selected {
                eprintln!("Frame browser: selection changed from {:?} to {:?}", prev_selected, self.monitor_selected_id);
                if let Some(id) = self.monitor_selected_id {
                    self.fetch_monitor_frame_on_demand(id);
                }
            }
        }

        ui.separator();
        ui.heading("Contrast");
        let (run_auto, run_region) = ui.horizontal(|ui| {
            ui.checkbox(&mut self.contrast.auto, "Auto");
            let run = ui.add_enabled(self.frame.is_some(), egui::Button::new("Run")).clicked();
            let region = ui.add_enabled(self.frame.is_some(), egui::Button::new("Region"))
                .on_hover_text("Auto contrast from visible region")
                .clicked();
            ui.checkbox(&mut self.auto_region, "Auto-region")
                .on_hover_text("Re-run region contrast after every pan or zoom");
            (run, region)
        }).inner;
        if run_auto {
            if let Some(frame) = self.frame.clone() {
                let (vmin, vmax) = auto_contrast(&frame);
                self.contrast.vmin = vmin;
                self.contrast.vmax = vmax;
            }
        }
        if run_region {
            if let Some(frame) = self.frame.clone() {
                let (vmin, vmax) = auto_contrast_region(&frame, &self.view, self.last_viewport_rect);
                self.contrast.vmin = vmin;
                self.contrast.vmax = vmax;
            }
        }

        let frame_max = self.effective_saturation() as f32;
        let vmin_max = (self.contrast.vmax - 1.0).max(1.0);
        ui.add_enabled(
            !self.contrast.auto,
            egui::Slider::new(&mut self.contrast.vmin, 0.0..=vmin_max).fixed_decimals(1).text("Background"),
        );
        ui.add_enabled(
            !self.contrast.auto,
            egui::Slider::new(&mut self.contrast.vmax, self.contrast.vmin..=frame_max).fixed_decimals(1).text("Foreground"),
        );
        ui.add(
            egui::Slider::new(&mut self.contrast.gamma_correction, 1.0..=10.0)
                .step_by(0.1)
                .text("Gamma"),
        );

        // Saturation override
        let sat_changed = ui.horizontal(|ui| {
            let before = (self.saturation_override_enabled, self.saturation_override_value);
            ui.checkbox(&mut self.saturation_override_enabled, "Force saturation");
            ui.add_enabled(
                self.saturation_override_enabled,
                egui::DragValue::new(&mut self.saturation_override_value).range(1..=u16::MAX),
            );
            (self.saturation_override_enabled, self.saturation_override_value) != before
        }).inner;
        if sat_changed {
            self.image_texture = crate::image_render::ImageTexture::default();
            let sat = self.effective_saturation();
            if let Some(ref mut frame) = self.frame {
                Arc::make_mut(frame).saturation_value = sat;
            }
        }

        // Histogram
        ui.horizontal(|ui| {
            ui.checkbox(&mut self.contrast.histogram_log, "Log");
            ui.add(
                egui::Slider::new(&mut self.contrast.histogram_bins, 32..=512)
                    .step_by(32.0)
                    .text("Bins"),
            );
        });
        let hist_height = 80.0;
        let (rect, _) =
            ui.allocate_exact_size(egui::vec2(ui.available_width(), hist_height), egui::Sense::hover());
        if ui.is_rect_visible(rect) {
            if let Some(ref frame) = self.frame {
                let n_bins = self.contrast.histogram_bins;
                let sat = self.effective_saturation();
                let frame_ptr = Arc::as_ptr(frame) as usize;
                let cache_valid = self.histogram_cache.as_ref()
                    .is_some_and(|(p, s, n, _)| *p == frame_ptr && *s == sat && *n == n_bins);
                if !cache_valid {
                    let mut counts = vec![0u32; n_bins];
                    for &v in &frame.pixels {
                        if v < sat {
                            let bin = ((v as usize * n_bins) / sat as usize).min(n_bins - 1);
                            counts[bin] += 1;
                        }
                    }
                    self.histogram_cache = Some((frame_ptr, sat, n_bins, counts));
                }
                let counts = &self.histogram_cache.as_ref().unwrap().3;
                let heights: Vec<f32> = counts
                    .iter()
                    .map(|&c| if self.contrast.histogram_log {
                        if c > 0 { (c as f32).ln() } else { 0.0 }
                    } else {
                        c as f32
                    })
                    .collect();
                let max_h = heights.iter().cloned().fold(0.0f32, f32::max).max(1.0);
                let painter = ui.painter();
                painter.rect_filled(rect, 0.0, egui::Color32::from_gray(20));
                let bar_w = rect.width() / n_bins as f32;
                for (i, &h) in heights.iter().enumerate() {
                    let norm = h / max_h;
                    let x0 = rect.left() + i as f32 * bar_w;
                    let y0 = rect.bottom() - norm * rect.height();
                    painter.rect_filled(
                        egui::Rect::from_min_max(
                            egui::pos2(x0, y0),
                            egui::pos2(x0 + bar_w + 0.5, rect.bottom()),
                        ),
                        0.0,
                        egui::Color32::from_gray(180),
                    );
                }
                // vmin / vmax markers
                let to_x = |v: f32| rect.left() + (v / sat as f32).clamp(0.0, 1.0) * rect.width();
                let stroke_vmin = egui::Stroke::new(1.5, egui::Color32::from_rgb(80, 200, 80));
                let stroke_vmax = egui::Stroke::new(1.5, egui::Color32::from_rgb(200, 80, 80));
                let vmin_x = to_x(self.contrast.vmin);
                let vmax_x = to_x(self.contrast.vmax);
                painter.line_segment([egui::pos2(vmin_x, rect.top()), egui::pos2(vmin_x, rect.bottom())], stroke_vmin);
                painter.line_segment([egui::pos2(vmax_x, rect.top()), egui::pos2(vmax_x, rect.bottom())], stroke_vmax);
            } else {
                ui.painter().rect_filled(rect, 0.0, egui::Color32::from_gray(20));
            }
        }

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
            let gamma = self.contrast.gamma_correction;
            let att = |c: u8| -> u8 { ((c as f32 / 255.0).powf(gamma) * 255.0).round() as u8 };
            for i in 0..n {
                let t = i as f32 / (n.saturating_sub(1)) as f32;
                let [r, g, b] = self.contrast.colormap.apply(t);
                let (r, g, b) = (att(r), att(g), att(b));
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
        ui.with_layout(egui::Layout::bottom_up(egui::Align::Min), |ui| {
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                ui.label(format!("Uptime {}", Self::format_uptime(self.start_time.elapsed())));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.link("Help (?)").clicked() {
                        self.show_help = true;
                    }
                    if ui.link("Actions…").clicked() {
                        self.show_actions = true;
                    }
                });
            });
            ui.separator();
        });
    }

    fn show_viewport(&mut self, ctx: &Context, ui: &mut Ui) {
        let available = ui.available_rect_before_wrap();
        self.last_viewport_rect = available;
        let response = ui.allocate_rect(available, egui::Sense::click_and_drag());

        let Some(ref frame) = self.frame.clone() else {
            if !self.splash_loaded {
                self.splash_loaded = true;
                if let Some(ref dir) = self.splash_folder.clone() {
                    if let Some(path) = Self::pick_random_png(dir) {
                        match std::fs::read(&path) {
                            Ok(bytes) => {
                                let image = Self::decode_splash_png(&bytes);
                                self.splash_texture = Some(ctx.load_texture(
                                    "splash",
                                    image,
                                    egui::TextureOptions::LINEAR,
                                ));
                            }
                            Err(e) => eprintln!("splash: cannot read {}: {e}", path.display()),
                        }
                    } else {
                        eprintln!("splash: no PNG files found in {}", dir.display());
                    }
                }
            }
            if let Some(ref texture) = self.splash_texture {
                let tex_size = texture.size_vec2();
                let scale = (available.width() / tex_size.x)
                    .min(available.height() / tex_size.y)
                    .min(1.0);
                let draw_size = tex_size * scale;
                let draw_rect = egui::Rect::from_center_size(available.center(), draw_size);
                ui.painter().image(
                    texture.id(),
                    draw_rect,
                    egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                    egui::Color32::WHITE,
                );
            }
            return;
        };

        // Center at 1:1 the first time a frame arrives.
        if self.pending_fit && self.hdf5_frame_index == 0 {
            self.view.zoom_to_one_centered(frame.width as f32, frame.height as f32, available);
            self.pending_fit = false;
        }

        // Handle pan + zoom input.
        let view_changed = viewport::handle_input(&mut self.view, &response, Some(frame), self.zoom_speed);
        if view_changed {
            if self.connected {
                self.last_interaction_time = Some(std::time::Instant::now());
            }
            if self.auto_region {
                if let Some(ref frame) = self.frame.clone() {
                    let (vmin, vmax) = auto_contrast_region(frame, &self.view, available);
                    self.contrast.vmin = vmin;
                    self.contrast.vmax = vmax;
                }
            }
        }

        let prefetched_id = if !self.on_demand_active {
            self.monitor_prefetcher
                .get(self.monitor_frame_index)
                .map(|h| h.id())
        } else {
            None
        };

        let texture_id = match prefetched_id {
            Some(id) => id,
            None => {
                let Some(t) = self.image_texture.update(
                    ctx,
                    frame,
                    self.frame_generation,
                    self.contrast.vmin,
                    self.contrast.vmax,
                    self.contrast.gamma_correction,
                    self.effective_saturation(),
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
        let open_hdf5_shortcut = KeyboardShortcut::new(Modifiers::COMMAND, Key::O);
        let fit_shortcut = KeyboardShortcut::new(Modifiers::COMMAND, Key::Num0);
        let zoom11_shortcut = KeyboardShortcut::new(Modifiers::COMMAND, Key::Num1);
        let help_shortcut = KeyboardShortcut::new(Modifiers::NONE, Key::Questionmark);
        let panel_shortcut = KeyboardShortcut::new(Modifiers::NONE, Key::Tab);
        let fullscreen_shortcut = KeyboardShortcut::new(Modifiers::NONE, Key::F11);

        if ctx.input_mut(|i| i.consume_shortcut(&help_shortcut)) {
            self.show_help = !self.show_help;
        }

        if ctx.input_mut(|i| i.consume_shortcut(&panel_shortcut)) {
            self.show_panel = !self.show_panel;
        }

        if ctx.input_mut(|i| i.consume_shortcut(&fullscreen_shortcut)) {
            let is_fullscreen = ctx.input(|i| i.viewport().fullscreen.unwrap_or(false));
            ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(!is_fullscreen));
        }

        if self.show_help {
            self.show_help_window(ctx);
        }

        self.show_actions_window(ctx);

        if ctx.input_mut(|i| i.consume_shortcut(&goto_shortcut)) {
            self.show_help = false;
            self.show_goto_frame = true;
            self.goto_frame_input.clear();
        }

        if ctx.input_mut(|i| i.consume_shortcut(&open_hdf5_shortcut)) {
            self.open_hdf5_dialog();
        }

        if ctx.input_mut(|i| i.consume_shortcut(&fit_shortcut)) {
            if let Some(ref frame) = self.frame {
                self.view.fit_to(frame.width as f32, frame.height as f32, self.last_viewport_rect);
            }
        }

        if ctx.input_mut(|i| i.consume_shortcut(&zoom11_shortcut)) {
            if self.frame.is_some() && self.last_viewport_rect.is_positive() {
                self.view.zoom_to_one(self.last_viewport_rect);
            }
        }

        if self.show_goto_frame {
            self.goto_frame(ctx);
        }

        if ctx.input_mut(|i| i.consume_shortcut(&quit_shortcut)) {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }

        let mouse_over_viewport = ctx.input(|i| {
            i.pointer.hover_pos().map_or(false, |p| self.last_viewport_rect.contains(p))
        });

        if let Some(ref series) = self.hdf5_series {
            let total = series.total_frames;
            let grouping = self.hdf5_grouping.max(1);
            let old_index = self.hdf5_frame_index;

            if mouse_over_viewport {
                if ctx.input_mut(|i| i.consume_shortcut(&previous_image_shortcut)) && self.hdf5_frame_index >= grouping {
                    self.hdf5_frame_index -= grouping;
                }

                if ctx.input_mut(|i| i.consume_shortcut(&next_image_shortcut)) && self.hdf5_frame_index + grouping < total {
                    self.hdf5_frame_index += grouping;
                }
            }

            if self.hdf5_frame_index != old_index {
                self.load_hdf5_grouped(self.hdf5_frame_index);
            }
        }

        // Poll monitor prefetcher and upload any completed textures.
        if self.monitor_prefetcher.poll(ctx) {
            ctx.request_repaint();
        }

        // Detect pause-expiry transition before anything else touches display state.
        let currently_paused = self.interaction_paused();
        let just_resumed = self.was_paused && !currently_paused;
        self.was_paused = currently_paused;

        // Invalidate monitor prefetcher cache when contrast settings change.
        let cur_contrast = (self.contrast.vmin, self.contrast.vmax, self.contrast.gamma_correction, self.contrast.colormap);
        if !self.monitor_frames.is_empty() && !self.on_demand_active && cur_contrast != self.monitor_contrast {
            self.monitor_prefetcher.invalidate();
            self.monitor_prefetcher.submit_batch(
                &self.monitor_frames,
                false,
                cur_contrast.0,
                cur_contrast.1,
                cur_contrast.2,
                self.effective_saturation(),
                cur_contrast.3,
            );
        }
        self.monitor_contrast = cur_contrast;

        let poll_got_frame = self.poll_new_frame();
        if poll_got_frame {
            ctx.request_repaint();
        }

        // Pause just expired and the monitor hasn't delivered a new batch yet —
        // immediately display the last cached frame rather than waiting up to
        // poll_period_ms for the next poll.
        if just_resumed && !poll_got_frame && !self.on_demand_active && !self.monitor_frames.is_empty() {
            self.monitor_prefetcher.invalidate();
            self.monitor_prefetcher.submit_batch(
                &self.monitor_frames,
                false,
                self.contrast.vmin,
                self.contrast.vmax,
                self.contrast.gamma_correction,
                self.effective_saturation(),
                self.contrast.colormap,
            );
            if let Some(frame) = self.monitor_frames.get(self.monitor_frame_index) {
                self.display_frame(frame.clone());
                ctx.request_repaint();
            }
        }

        // Receive on-demand frames requested from the frame browser pulldown.
        if let Ok(frame) = self.on_demand_rx.try_recv() {
            eprintln!("On-demand: received frame, displaying image #{:?}", frame.metadata.image_number);
            self.monitor_prefetcher.invalidate();
            self.on_demand_active = true;
            self.display_frame(Arc::new(frame));
            ctx.request_repaint();
        }

        // Record activity on any key press, pointer click, or scroll wheel event.
        // Mouse movement alone is ignored — it doesn't indicate intent to keep
        // monitoring active.
        let has_activity = ctx.input(|i| {
            !i.events.is_empty() && i.events.iter().any(|e| matches!(
                e,
                egui::Event::Key { .. }
                | egui::Event::PointerButton { .. }
                | egui::Event::MouseWheel { .. }
                | egui::Event::Zoom(_)
            ))
        });
        if has_activity {
            self.last_activity_time = std::time::Instant::now();
        }

        // Adjust the background poll rate based on window visibility/focus, and
        // keep the UI ticking so new frames are displayed promptly.
        if self.connected {
            let minimized = ctx.input(|i| i.viewport().minimized.unwrap_or(false));
            let focused   = ctx.input(|i| i.viewport().focused.unwrap_or(true));
            let idle      = self.is_idle_paused();

            let desired_period = if minimized || idle {
                0 // paused — no fetches while hidden or idle
            } else if focused {
                self.poll_period_ms
            } else {
                self.unfocused_poll_period_ms
            };

            if let Some(ref tx) = self.monitor_ctl_tx {
                let _ = tx.send(desired_period);
            }

            // Only request repaints when the window can actually be seen.
            if !minimized {
                ctx.request_repaint_after(std::time::Duration::from_millis(50));
            }
        }

        // While paused, schedule a repaint at the moment the pause expires so
        // the "⏸ paused" label disappears and live updates resume promptly.
        if let Some(t) = self.last_interaction_time {
            if self.monitor_pause_ms > 0 {
                let elapsed = t.elapsed().as_millis() as u64;
                if elapsed < self.monitor_pause_ms {
                    let remaining = self.monitor_pause_ms - elapsed;
                    ctx.request_repaint_after(std::time::Duration::from_millis(remaining));
                }
            }
        }

        self.file_dialog.update(ctx);
        if let Some(path) = self.file_dialog.take_picked() {
            eprintln!("Loading HDF5: {}", path.display());
            self.disconnect();
            if let Some(parent) = path.parent() {
                self.last_location = Some(parent.to_path_buf());
                self.save_last_location();
            }
            if let Err(e) = self.load_hdf5_master(&path) {
                eprintln!("Failed to open {}: {e}", path.display());
            }
        }

        SidePanel::left("left_panel").resizable(true).default_width(240.0).show_animated(ctx, self.show_panel, |ui| {
            ScrollArea::vertical().show(ui, |ui| {
                self.show_left_panel(ui);
            });
        });

        CentralPanel::default().show(ctx, |ui| {
            self.show_viewport(ctx, ui);
        });
    }
}

/// Same algorithm as `auto_contrast` but restricted to the pixels currently
/// visible in the viewport. Falls back to the full-frame version if the
/// visible region is empty or entirely saturated.
fn auto_contrast_region(frame: &Frame, view: &ViewState, viewport: egui::Rect) -> (f32, f32) {
    let x0 = view.offset.x.max(0.0) as u32;
    let y0 = view.offset.y.max(0.0) as u32;
    let x1 = (view.offset.x + viewport.width() / view.zoom).min(frame.width as f32) as u32;
    let y1 = (view.offset.y + viewport.height() / view.zoom).min(frame.height as f32) as u32;

    if x1 <= x0 || y1 <= y0 {
        return auto_contrast(frame);
    }

    let sat = frame.saturation_value;
    let mut vals: Vec<u16> = (y0..y1)
        .flat_map(|y| (x0..x1).map(move |x| frame.pixels[(y * frame.width + x) as usize]))
        .filter(|&v| v < sat)
        .collect();

    if vals.is_empty() {
        return auto_contrast(frame);
    }

    vals.sort_unstable();
    let n = vals.len();
    let vmin = vals[(n as f32 * 0.001) as usize];
    let max_val = *vals.last().unwrap() as f32;
    let vmax = (max_val * 0.010).max(5.0);
    (vmin as f32, vmax)
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

    let n = vals.len();
    let p01 = vals[(n as f32 * 0.001) as usize];
    let max_val = *vals.last().unwrap() as f32;
    let vmax = (max_val * 0.010).max(p01 as f32 + 5.0);
    eprintln!(
        "auto_contrast: n={n} sat_filtered={:.1}% p01={p01} max={max_val:.0} → vmin={:.1} vmax={:.1}",
        n as f32 / frame.pixels.len() as f32 * 100.0,
        p01 as f32,
        vmax,
    );
    (p01 as f32, vmax)
}
