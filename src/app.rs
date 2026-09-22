use std::sync::Arc;
use std::time::Duration;

use egui::{CentralPanel, Context, Key, KeyboardShortcut, Modifiers, ScrollArea, SidePanel, Ui};
use egui_file_dialog::FileDialog;
use tokio::sync::watch;

use crate::contrast::{ContrastState, AUTO_CONTRAST_MIN_SPAN, auto_contrast, auto_contrast_region};
use crate::frame::{Frame, FrameMetadata};
use crate::frame_loader::{FrameLoader, LoadKey};
use crate::hdf5_loader::Hdf5Series;
use crate::line_profile::{self, LineProfilePeak};
use crate::image_render::{Colormap, ImageTexture, ToneMapParams};
use crate::monitor::{MonitorBatch, MonitorConfig, start_monitor_task};
use crate::monitor_prefetch::MonitorPrefetcher;
use crate::viewport::{self, OverlaySettings, ViewState};

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
    monitor_contrast: Option<ToneMapParams>,
    /// The prefetch cache was invalidated by a contrast change and not yet refilled.
    monitor_prefetch_pending: bool,
    /// True while the user is viewing a specific on-demand frame from the browser;
    /// suppresses prefetcher use and batch re-submission so the cached monitor
    /// texture never overwrites what the user explicitly selected.
    on_demand_active: bool,

    /// Open HDF5 series, if any.
    hdf5_series: Option<Hdf5Series>,
    /// Background reader/cache for the open series; `None` when no series is open.
    hdf5_loader: Option<FrameLoader>,
    /// The frame the user last asked for that has not arrived yet.
    hdf5_awaiting: Option<LoadKey>,
    egui_ctx: egui::Context,
    /// Path to the master file (kept for the prefetch thread to open its own handle).
    hdf5_master_path: Option<std::path::PathBuf>,
    /// Current frame index within the HDF5 series (always a multiple of hdf5_grouping).
    hdf5_frame_index: usize,
    /// Number of consecutive frames to sum before displaying.
    hdf5_grouping: usize,
    /// Movie mode: auto-advance through the HDF5 series (never active in monitor mode).
    movie_playing: bool,
    movie_fps: f32,
    movie_last_advance: std::time::Instant,

    saturation_override_enabled: bool,
    saturation_override_value: u16,
    /// Original saturation_value from the file/detector, before any override.
    file_saturation_value: Option<u16>,

    /// Cached histogram counts. Invalidated when frame, saturation, or bin count changes.
    histogram_cache: Option<(usize, u16, usize, Vec<u32>)>, // (frame_ptr, saturation, n_bins, counts)

    // goto a given frame by number
    show_goto_frame: bool,
    goto_frame_input: String,
    goto_frame_needs_focus: bool,

    auto_region: bool,
    lock_zoom: bool,
    zoom_speed: f32,
    /// Which accordion section is currently open: 0 = Data browser, 1 = Current dataset.
    active_panel: usize,
    show_help: bool,
    show_panel: bool,
    show_actions: bool,
    /// Output size of "Save PNG" as a percentage of the frame size.
    save_scale: u32,
    /// Output size of "Copy image" as a percentage of the frame size.
    copy_scale: u32,
    /// Save/copy only the part of the image currently visible in the viewport.
    visible_only: bool,

    start_time: std::time::Instant,

    splash_folder: Option<std::path::PathBuf>,
    splash_texture: Option<egui::TextureHandle>,
    splash_loaded: bool,

    /// Last folder used to open a file.
    last_location: Option<std::path::PathBuf>,

    file_dialog: FileDialog,

    data_browser: Option<crate::data_browser::DataBrowser>,

    dozor_data: Option<crate::dozor::DozorData>,
    dozor_collapsed: bool,

    // Line profile (right-button drag)
    line_profile_start: Option<egui::Pos2>,  // image-space
    line_profile_end: Option<egui::Pos2>,    // image-space
    line_profile_data: Vec<f32>,
    line_profile_width: u32,

    remote_rx: tokio::sync::mpsc::UnboundedReceiver<crate::remote::RemoteRequest>,
    commands_file_enabled: bool,
    commands_file_path: String,
    commands_file_poll_interval_ms: u64,
    commands_file_watcher: Option<crate::remote::CommandsFileWatcher>,
    commands_file_rx: Option<tokio::sync::mpsc::UnboundedReceiver<crate::remote::RemoteCmd>>,
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
        cc: &eframe::CreationContext,
        dcu_url: String,
        poll_period_ms: u64,
        unfocused_poll_period_ms: u64,
        monitor_pause_ms: u64,
        idle_pause_secs: u64,
        auto_connect: bool,
        contrast: ContrastState,
        overlays: OverlaySettings,
        splash_folder: Option<std::path::PathBuf>,
        remote_rx: tokio::sync::mpsc::UnboundedReceiver<crate::remote::RemoteRequest>,
        commands_file: Option<std::path::PathBuf>,
        commands_file_enabled: bool,
        commands_file_poll_interval_ms: u64,
        data_browser_cfg: Option<crate::config::DataBrowserConfig>,
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
            monitor_contrast: None,
            monitor_prefetch_pending: false,
            on_demand_active: false,
            hdf5_series: None,
            hdf5_loader: None,
            hdf5_awaiting: None,
            egui_ctx: cc.egui_ctx.clone(),
            hdf5_master_path: None,
            hdf5_frame_index: 0,
            hdf5_grouping: 1,
            movie_playing: false,
            movie_fps: 10.0,
            movie_last_advance: std::time::Instant::now(),
            saturation_override_enabled: true,
            saturation_override_value: 32767,
            file_saturation_value: None,
            histogram_cache: None,
            show_goto_frame: false,
            goto_frame_input: "0".to_string(),
            goto_frame_needs_focus: false,
            auto_region: false,
            lock_zoom: true,
            zoom_speed: 0.006,
            active_panel: 1,
            show_help: false,
            show_panel: true,
            show_actions: false,
            save_scale: 100,
            copy_scale: 50,
            visible_only: false,
            start_time: std::time::Instant::now(),
            splash_folder,
            splash_texture: None,
            splash_loaded: false,
            last_location: Self::load_last_location(),
            file_dialog: FileDialog::new()
                .add_file_filter_extensions("HDF5 master", vec!["h5"]),
            data_browser: data_browser_cfg.map(crate::data_browser::DataBrowser::new),
            dozor_data: None,
            dozor_collapsed: false,
            line_profile_start: None,
            line_profile_end: None,
            line_profile_data: Vec::new(),
            line_profile_width: 3,
            remote_rx,
            commands_file_enabled: false,
            commands_file_path: commands_file
                .as_ref()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default(),
            commands_file_poll_interval_ms,
            commands_file_watcher: None,
            commands_file_rx: None,
        };
        if commands_file_enabled {
            app.set_commands_file_enabled(true);
        }
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

    fn decode_splash_png(bytes: &[u8]) -> anyhow::Result<egui::ColorImage> {
        use anyhow::Context as _;
        let decoder = png::Decoder::new(std::io::Cursor::new(bytes));
        let mut reader = decoder.read_info().context("splash PNG info")?;
        let mut buf = vec![0u8; reader.output_buffer_size().context("splash PNG buffer size")?];
        let info = reader.next_frame(&mut buf).context("splash PNG frame")?;
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
            t => anyhow::bail!("unsupported splash PNG color type: {t:?}"),
        };
        Ok(egui::ColorImage::new([width, height], pixels))
    }

    pub fn load_hdf5_master(&mut self, path: &std::path::Path) -> anyhow::Result<()> {
        let series = Hdf5Series::open(path)?;
        let first = series.load_frame(0)?;
        self.hdf5_master_path = Some(path.to_path_buf());
        self.hdf5_series = Some(series);
        self.hdf5_loader = Some(FrameLoader::spawn(path.to_path_buf(), self.egui_ctx.clone()));
        self.hdf5_awaiting = None;
        self.hdf5_frame_index = 0;
        self.on_new_frame(Arc::new(first));
        self.dozor_data = crate::dozor::find_dozor_json(path)
            .and_then(|p| crate::dozor::load_dozor(&p));
        self.dozor_collapsed = false;
        Ok(())
    }

    /// Start or stop movie mode. Only available for an HDF5 series outside monitor mode.
    /// Starting on the last group rewinds to the first frame.
    fn toggle_movie(&mut self) {
        if self.movie_playing {
            self.movie_playing = false;
            return;
        }
        self.start_movie_playback();
    }

    /// Begin movie playback at `self.movie_fps`. Only available for an HDF5 series
    /// outside monitor mode. Starting on the last group rewinds to the first frame.
    fn start_movie_playback(&mut self) {
        if self.connected {
            return;
        }
        self.start_movie_playback_inner();
    }

    /// Like `start_movie_playback`, but for remote commands: allowed to start even
    /// while `connected` to the live monitor, since the on-demand display (already
    /// showing a remote-selected HDF5 frame) holds regardless of the live feed.
    fn start_movie_playback_remote(&mut self) {
        self.start_movie_playback_inner();
    }

    fn start_movie_playback_inner(&mut self) {
        let Some(total) = self.hdf5_series.as_ref().map(|s| s.total_frames) else { return };
        let grouping = self.hdf5_grouping.max(1);
        if self.hdf5_frame_index + grouping >= total && total > grouping {
            self.hdf5_frame_index = 0;
            self.load_hdf5_grouped(0);
        }
        self.movie_playing = true;
        self.movie_last_advance = std::time::Instant::now();
    }

    /// Stop movie playback, discard the on-demand HDF5 series, and resume showing
    /// the live-monitored frames.
    fn stop_movie_and_resume_monitoring(&mut self) {
        self.movie_playing = false;
        self.on_demand_active = false;
        self.hdf5_series = None;
        self.hdf5_loader = None;
        self.hdf5_awaiting = None;
        self.hdf5_master_path = None;
        self.monitor_selected_id = self.monitor_image_ids.last().copied();
        self.monitor_frame_index = self.monitor_frames.len().saturating_sub(1);
        self.monitor_prefetcher.invalidate();
        if let Some(frame) = self.monitor_frames.get(self.monitor_frame_index) {
            self.display_frame(frame.clone());
        }
    }

    /// Show the frame (or group of `hdf5_grouping` frames) starting at `start_index`.
    ///
    /// Cached frames are shown immediately; otherwise the read happens on the
    /// loader thread and `poll_hdf5_loader` displays the result when it arrives.
    fn load_hdf5_grouped(&mut self, start_index: usize) {
        let key = LoadKey::new(start_index, self.hdf5_grouping, self.effective_saturation());
        let Some(ref loader) = self.hdf5_loader else { return };
        if let Some(frame) = loader.get(key) {
            self.hdf5_awaiting = None;
            self.on_new_frame(frame);
            self.prefetch_after(key);
        } else {
            self.hdf5_awaiting = Some(key);
            loader.request(key);
        }
    }

    /// Read ahead the group after `key` so stepping or playing forward is instant.
    fn prefetch_after(&self, key: LoadKey) {
        let (Some(loader), Some(series)) = (&self.hdf5_loader, &self.hdf5_series) else { return };
        let next = key.start + key.grouping;
        if next < series.total_frames {
            loader.prefetch(LoadKey::new(next, self.hdf5_grouping, self.effective_saturation()));
        }
    }

    /// Display the awaited frame once the loader thread delivers it.
    fn poll_hdf5_loader(&mut self) {
        let Some(ref mut loader) = self.hdf5_loader else { return };
        for loaded in loader.poll() {
            match loaded.result {
                Ok(frame) if self.hdf5_awaiting == Some(loaded.key) => {
                    self.hdf5_awaiting = None;
                    self.on_new_frame(frame);
                    self.prefetch_after(loaded.key);
                }
                Ok(_) => {}
                Err(e) => {
                    if self.hdf5_awaiting == Some(loaded.key) {
                        self.hdf5_awaiting = None;
                    }
                    eprintln!("HDF5 frame {}: {e}", loaded.key.start);
                }
            }
        }
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
    /// Current tone-mapping parameters (contrast settings plus effective saturation).
    fn tone_params(&self) -> ToneMapParams {
        ToneMapParams {
            vmin: self.contrast.vmin,
            vmax: self.contrast.vmax,
            gamma: self.contrast.gamma_correction,
            saturation: self.effective_saturation(),
            colormap: self.contrast.colormap,
        }
    }

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

        let mut open = true;
        egui::Window::new("Go to frame")
            .collapsible(false)
            .resizable(false)
            .open(&mut open)
            .show(ctx, |ui| {
                ui.label("Frame number:");

                let response = ui.text_edit_singleline(&mut self.goto_frame_input);

                if self.goto_frame_needs_focus {
                    response.request_focus();
                    self.goto_frame_needs_focus = false;
                }

                if response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    if let Ok(frame) = self.goto_frame_input.parse::<usize>() {
                        self.do_goto_frame(frame);
                    }
                    self.show_goto_frame = false;
                }

                // Esc cancels
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
        // X button sets open=false; buttons/keys set show_goto_frame=false; both close the dialog.
        self.show_goto_frame = self.show_goto_frame && open;
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

        let mut open = self.show_help;
        egui::Window::new("Help")
            .collapsible(false)
            .resizable(false)
            .open(&mut open)
            .show(ctx, |ui| {
                ui.heading("Keyboard Shortcuts");
                egui::Grid::new("help_grid").num_columns(2).spacing([20.0, 8.0]).show(ui, |ui| {
                    ui.label("Ctrl-0"); ui.label("Fit image to view"); ui.end_row();
                    ui.label("Ctrl-1"); ui.label("Zoom to 1:1"); ui.end_row();
                    ui.label("Left / Right"); ui.label("Previous / Next frame"); ui.end_row();
                    ui.label("Ctrl+O"); ui.label("Open HDF5 master"); ui.end_row();
                    ui.label("Ctrl+G"); ui.label("Go to frame number"); ui.end_row();
                    ui.label("Ctrl+S"); ui.label("Save current image as PNG"); ui.end_row();
                    ui.label("Ctrl+C"); ui.label("Copy current image to clipboard"); ui.end_row();
                    ui.label("Ctrl+Q"); ui.label("Quit"); ui.end_row();
                    ui.label("Tab"); ui.label("Hide / show side panel"); ui.end_row();
                    ui.label("Ctrl+P"); ui.label("Play / stop movie (HDF5 only)"); ui.end_row();
                    ui.label("Ctrl+R"); ui.label("Toggle resolution rings"); ui.end_row();
                    ui.label("F11"); ui.label("Toggle fullscreen"); ui.end_row();
                    ui.label("?"); ui.label("Show this help"); ui.end_row();
                    ui.label("Hold F + left-drag"); ui.label("Adjust contrast (Foreground)"); ui.end_row();
                });

                ui.add_space(10.0);
                ui.separator();
                ui.label(format!("Pumpkin v{}", env!("APP_VERSION")));
            });

        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            open = false;
        }
        self.show_help = open;
    }

    fn show_actions_window(&mut self, ctx: &Context) {
        if !self.show_actions {
            return;
        }

        let mut open = self.show_actions;
        egui::Window::new("Actions")
            .collapsible(false)
            .resizable(false)
            .open(&mut open)
            .show(ctx, |ui| {
                ui.heading("Open file");
                ui.horizontal(|ui| {
                    if ui.button("Open HDF5…").on_hover_text("Open HDF5 master file (Ctrl+O)").clicked() {
                        self.open_hdf5_dialog();
                    }
                    let save_enabled = self.frame.is_some();
                    if ui.add_enabled(save_enabled, egui::Button::new("Save PNG"))
                        .on_hover_text("Save current image with overlays as PNG (Ctrl+S)")
                        .clicked()
                    {
                        self.save_png();
                    }
                    if ui.add_enabled(save_enabled, egui::Button::new("Copy image"))
                        .on_hover_text("Copy current image with overlays to the clipboard (Ctrl+C)")
                        .clicked()
                    {
                        self.copy_image(ctx);
                    }
                });
                ui.horizontal(|ui| {
                    ui.label("Save scaling factor:");
                    ui.add(egui::Slider::new(&mut self.save_scale, 10..=100).step_by(10.0).suffix("%"));
                });
                ui.horizontal(|ui| {
                    ui.label("Copy scaling factor:");
                    ui.add(egui::Slider::new(&mut self.copy_scale, 10..=100).step_by(10.0).suffix("%"));
                });
                ui.checkbox(&mut self.visible_only, "Only visible pixels")
                    .on_hover_text("Save/copy just the part of the image shown in the viewport");
                if let Some(db) = self.data_browser.as_mut() {
                    let mut max = db.recent_max();
                    ui.horizontal(|ui| {
                        ui.label("Recent monitored files kept:");
                        if ui.add(egui::DragValue::new(&mut max).range(0..=200))
                            .on_hover_text("Older entries are dropped from the list and from disk (config: recent_monitored_max)")
                            .changed()
                        {
                            db.set_recent_max(max);
                        }
                    });
                }

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
                ui.heading("Commands file");
                let mut enabled = self.commands_file_enabled;
                if ui.checkbox(&mut enabled, "Enabled").changed() {
                    self.set_commands_file_enabled(enabled);
                }
                egui::Grid::new("commands_file_grid").num_columns(2).show(ui, |ui| {
                    ui.label("Path");
                    let path_resp = ui.text_edit_singleline(&mut self.commands_file_path);
                    ui.end_row();
                    ui.label("Interval");
                    let interval_resp = ui.add(
                        egui::DragValue::new(&mut self.commands_file_poll_interval_ms)
                            .speed(50.0)
                            .range(50..=60_000)
                            .suffix(" ms"),
                    );
                    ui.end_row();
                    if (path_resp.lost_focus() || interval_resp.changed()) && self.commands_file_enabled {
                        self.restart_commands_file_watcher();
                    }
                });

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
                        egui::Slider::new(&mut self.zoom_speed, 0.001..=0.1)
                            .step_by(0.001)
                            .fixed_decimals(3),
                    );
                    ui.end_row();
                });

                ui.add_space(8.0);
                ui.heading("Line profile");
                egui::Grid::new("line_profile_grid").num_columns(2).show(ui, |ui| {
                    ui.label("Width");
                    ui.add(egui::DragValue::new(&mut self.line_profile_width).range(1u32..=200).suffix(" px"));
                    ui.end_row();
                });
            });
        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            open = false;
        }
        self.show_actions = open;
    }

    fn set_commands_file_enabled(&mut self, enabled: bool) {
        if enabled {
            self.commands_file_enabled = true;
            self.restart_commands_file_watcher();
        } else {
            self.commands_file_enabled = false;
            self.stop_commands_file_watcher();
        }
    }

    fn restart_commands_file_watcher(&mut self) {
        self.stop_commands_file_watcher();
        if !self.commands_file_enabled {
            return;
        }
        let path_text = self.commands_file_path.trim();
        if path_text.is_empty() {
            eprintln!("Commands file: no path configured");
            self.commands_file_enabled = false;
            return;
        }
        let poll_interval = Duration::from_millis(self.commands_file_poll_interval_ms.max(50));
        let cfg = crate::remote::CommandsFileConfig {
            path: std::path::PathBuf::from(path_text),
            poll_interval,
        };
        let (watcher, rx) = crate::remote::start_commands_file_watcher(cfg, self.egui_ctx.clone());
        self.commands_file_watcher = Some(watcher);
        self.commands_file_rx = Some(rx);
    }

    fn stop_commands_file_watcher(&mut self) {
        self.commands_file_rx = None;
        if let Some(watcher) = self.commands_file_watcher.take() {
            watcher.stop();
        }
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

    /// Load `path` as the HDF5 master, unless it's already the one loaded. Returns
    /// `Err` (after logging) if loading fails.
    fn ensure_hdf5_master(&mut self, path: &std::path::Path) -> Result<(), String> {
        if self.hdf5_master_path.as_deref() == Some(path) {
            return Ok(());
        }
        self.load_hdf5_master(path).map_err(|e| {
            let msg = format!("failed to load {}: {e:#}", path.display());
            eprintln!("Remote: {msg}");
            msg
        })
    }

    fn handle_remote_cmd(&mut self, cmd: crate::remote::RemoteCmd) -> Result<(), String> {
        match cmd {
            crate::remote::RemoteCmd::LoadFrame { file, frame } => {
                self.ensure_hdf5_master(&file)?;
                let grouping = self.hdf5_grouping.max(1);
                let snapped = (frame / grouping) * grouping;
                let snapped = if let Some(ref s) = self.hdf5_series {
                    snapped.min(s.total_frames.saturating_sub(1))
                } else {
                    snapped
                };
                self.hdf5_frame_index = snapped;
                self.load_hdf5_grouped(snapped);
                // Hold the live monitor display in place while showing a remote HDF5 frame.
                // on_monitor_batch clears this when a new detector series is detected.
                self.on_demand_active = true;
                Ok(())
            }
            crate::remote::RemoteCmd::PlayMovie { file: _, fps: 0 } => {
                self.stop_movie_and_resume_monitoring();
                Ok(())
            }
            crate::remote::RemoteCmd::PlayMovie { file, fps } => {
                self.ensure_hdf5_master(&file)?;
                self.movie_fps = fps as f32;
                self.on_demand_active = true;
                self.start_movie_playback_remote();
                if !self.movie_playing {
                    return Err("could not start movie playback (no frames in series?)".to_string());
                }
                Ok(())
            }
        }
    }

    fn on_monitor_batch(&mut self, batch: MonitorBatch) {
        let new_series = Some(batch.series_id) != self.monitor_series_id;
        self.monitor_series_id = Some(batch.series_id);
        self.monitor_image_ids = batch.image_ids;
        self.monitor_series_meta = batch.metadata;
        self.monitor_frames = batch.frames;

        if new_series {
            if let (Some(db), Some(pattern)) = (self.data_browser.as_mut(), self.monitor_series_meta.name_pattern.as_deref()) {
                db.record_monitored(pattern, batch.series_id);
            }
            // New series: always go live, discard any on-demand or remote HDF5 selection.
            self.on_demand_active = false;
            self.hdf5_series = None;
            self.hdf5_loader = None;
            self.hdf5_awaiting = None;
            self.hdf5_master_path = None;
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
            self.tone_params(),
        );

        if let Some(frame) = self.monitor_frames.get(self.monitor_frame_index) {
            self.display_frame(frame.clone());
        }
    }

    fn connect(&mut self) {
        self.movie_playing = false;
        self.hdf5_series = None;
        self.hdf5_loader = None;
        self.hdf5_awaiting = None;
        self.hdf5_master_path = None;
        self.dozor_data = None;
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
        self.dozor_data = None;
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

    /// Image-pixel region currently visible in the viewport, clipped to the frame.
    /// `Ok(None)` means "use the whole image" (option off); `Err` means nothing is visible.
    fn output_crop(&self, frame: &Frame) -> Result<Option<crate::png_export::CropRect>, &'static str> {
        if !self.visible_only {
            return Ok(None);
        }
        let vp = self.last_viewport_rect;
        if !vp.is_positive() {
            return Err("viewport size unknown");
        }
        let min = self.view.screen_to_image(vp.min, vp.min);
        let max = self.view.screen_to_image(vp.max, vp.min);
        let x0 = min.x.floor().max(0.0) as u32;
        let y0 = min.y.floor().max(0.0) as u32;
        let x1 = (max.x.ceil().max(0.0) as u32).min(frame.width);
        let y1 = (max.y.ceil().max(0.0) as u32).min(frame.height);
        if x1 <= x0 || y1 <= y0 {
            return Err("no part of the image is visible");
        }
        Ok(Some(crate::png_export::CropRect { x0, y0, x1, y1 }))
    }

    fn save_png(&self) {
        let Some(ref frame) = self.frame else { return };
        let crop = match self.output_crop(frame) {
            Ok(c) => c,
            Err(why) => {
                eprintln!("PNG export skipped: {why}");
                return;
            }
        };
        let save_dir = std::env::var_os("HOME")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| std::path::PathBuf::from("."));
        match crate::png_export::export_png(
            frame,
            self.tone_params(),
            &self.overlays,
            &save_dir,
            crop,
            self.save_scale,
        ) {
            Ok(path) => eprintln!("Saved PNG: {}", path.display()),
            Err(e)   => eprintln!("PNG export failed: {e:#}"),
        }
    }

    fn copy_image(&self, ctx: &egui::Context) {
        let Some(ref frame) = self.frame else { return };
        let crop = match self.output_crop(frame) {
            Ok(c) => c,
            Err(why) => {
                eprintln!("Copy skipped: {why}");
                return;
            }
        };
        ctx.copy_image(crate::png_export::render_color_image(
            frame,
            self.tone_params(),
            &self.overlays,
            crop,
            self.copy_scale,
        ));
        eprintln!("Copied image to clipboard");
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

    /// Draw a styled accordion header. Returns true if it was clicked.
    fn accordion_header(ui: &mut Ui, title: &str, is_open: bool) -> bool {
        let height = 30.0;
        let closed_bg = egui::Color32::from_rgb(0x76, 0x07, 0x59);
        let open_bg   = egui::Color32::from_rgb(0x9e, 0x1a, 0x80);
        let hover_bg  = egui::Color32::from_rgb(0x86, 0x17, 0x69);
        let (rect, response) = ui.allocate_exact_size(
            egui::vec2(ui.available_width(), height),
            egui::Sense::click(),
        );
        response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, title));

        if ui.is_rect_visible(rect) {
            let fill = if is_open {
                open_bg
            } else if response.hovered() {
                hover_bg
            } else {
                closed_bg
            };
            ui.painter().rect_filled(rect, 0.0, fill);

            // Draw focus indicator
            if response.has_focus() {
                ui.painter().rect_stroke(rect, 0.0, ui.visuals().selection.stroke, egui::StrokeKind::Inside);
            }

            ui.painter().text(
                rect.left_center() + egui::vec2(10.0, 0.0),
                egui::Align2::LEFT_CENTER,
                title,
                egui::FontId::proportional(13.0),
                egui::Color32::WHITE,
            );
        }
        response.clicked()
    }

    fn show_left_panel(&mut self, ui: &mut Ui) -> Option<std::path::PathBuf> {
        let mut panel_action: Option<std::path::PathBuf> = None;

        // -- Current dataset --
        if Self::accordion_header(ui, "Current dataset", self.active_panel == 1) { self.active_panel = 1; }
        if self.active_panel == 1 {

        ui.add_space(4.0);
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
                row!("Exposure", meta.exposure_time.map_or("-".into(), |v| format!("{v:.4} s")));
                if let Some(n) = meta.nimages {
                    row!("# images", n.to_string());
                }
                if let Some(n) = meta.ntrigger {
                    row!("# triggers", n.to_string());
                }
                if let Some(n) = meta.image_number {
                    row!("Image #", n.to_string());
                }
                if let Some(ref p) = meta.name_pattern {
                    if let Some(name) = crate::text::display_name(p) {
                        row!("Name", name.to_string());
                    }
                }
                if let Some(ref d) = meta.data_collection_date {
                    row!("Collect date", d.clone());
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
            let mut toggle_movie = false;

            let mut group_idx = self.hdf5_frame_index / grouping;
            if ui.add(egui::Slider::new(&mut group_idx, 0..=n_groups.saturating_sub(1)).text("group")).changed() {
                self.hdf5_frame_index = group_idx * grouping;
            }

            ui.horizontal(|ui| {
                let btn_size = egui::vec2(24.0, 24.0);
                if ui.add(egui::Button::new("◀").min_size(btn_size)).on_hover_text("Previous frame").clicked() && self.hdf5_frame_index >= grouping {
                    self.hdf5_frame_index -= grouping;
                }
                if ui.add(egui::Button::new("▶").min_size(btn_size)).on_hover_text("Next frame").clicked() && self.hdf5_frame_index + grouping < total {
                    self.hdf5_frame_index += grouping;
                }
                if ui.add(egui::Button::new("|◀").min_size(btn_size)).on_hover_text("First frame").clicked() {
                    self.hdf5_frame_index = 0;
                }
                if ui.add(egui::Button::new("▶|").min_size(btn_size)).on_hover_text("Last frame").clicked() {
                    self.hdf5_frame_index = (n_groups - 1) * grouping;
                }
                ui.separator();
                let label = if self.movie_playing { "⏸ Stop" } else { "🎞 Movie" };
                if ui.add_enabled(!self.connected, egui::Button::new(label))
                    .on_hover_text("Play through the series (Ctrl+P)")
                    .clicked()
                {
                    toggle_movie = true;
                }
                ui.add(egui::DragValue::new(&mut self.movie_fps).range(0.5..=60.0).speed(0.1).suffix(" fps"));
            });

            if self.hdf5_frame_index != old_index || self.hdf5_grouping != grouping {
                self.load_hdf5_grouped(self.hdf5_frame_index);
            }
            if toggle_movie {
                self.toggle_movie();
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
                if ui.add_enabled(can_prev, egui::Button::new(egui::RichText::new("◀").size(36.0)).min_size(egui::vec2(80.0, 60.0)).corner_radius(10))
                    .on_hover_text("Previous frame")
                    .clicked()
                {
                    self.monitor_selected_id = Some(self.monitor_image_ids[cur_pos.unwrap() - 1]);
                }
                if ui.add_enabled(can_next, egui::Button::new(egui::RichText::new("▶").size(36.0)).min_size(egui::vec2(80.0, 60.0)).corner_radius(10))
                    .on_hover_text("Next frame")
                    .clicked()
                {
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
            ui.checkbox(&mut self.contrast.auto, "Auto")
                .on_hover_text("Recompute Background/Foreground from the whole image on every new frame");
            let run = ui.add_enabled(self.frame.is_some(), egui::Button::new("Run"))
                .on_hover_text("Recompute Background/Foreground from the whole image now")
                .clicked();
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
            egui::Slider::new(&mut self.contrast.vmin, 0.0..=vmin_max)
                .logarithmic(true)
                .smallest_positive(1.0)
                .fixed_decimals(1)
                .text("Background"),
        ).on_hover_text("Pixel value mapped to the darkest colour (black point)");
        ui.add_enabled(
            !self.contrast.auto,
            egui::Slider::new(&mut self.contrast.vmax, self.contrast.vmin..=frame_max)
                .logarithmic(true)
                .smallest_positive(1.0)
                .fixed_decimals(1)
                .text("Foreground"),
        ).on_hover_text("Pixel value mapped to the brightest colour (white point); capped by the saturation threshold");
        ui.add(
            egui::Slider::new(&mut self.contrast.gamma_correction, 1.0..=10.0)
                .step_by(0.1)
                .text("Gamma"),
        ).on_hover_text("Gamma correction applied after the colormap");

        // Saturation override
        let sat_changed = ui.horizontal(|ui| {
            let before = (self.saturation_override_enabled, self.saturation_override_value);
            ui.checkbox(&mut self.saturation_override_enabled, "Force saturation")
                .on_hover_text("Override the detector-reported saturation threshold used to clip overflowed pixels");
            ui.add_enabled(
                self.saturation_override_enabled,
                egui::DragValue::new(&mut self.saturation_override_value).range(1..=u16::MAX),
            ).on_hover_text("Saturation threshold (counts); pixels at or above this render as black");
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
            ui.checkbox(&mut self.contrast.histogram_log, "Log bins")
                .on_hover_text("Scale histogram bar heights logarithmically (display only, does not affect the image)");
            ui.add(
                egui::Slider::new(&mut self.contrast.histogram_bins, 32..=512)
                    .step_by(32.0)
                    .text("Bins"),
            ).on_hover_text("Number of histogram bins");
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
                    for (i, &v) in frame.pixels.iter().enumerate() {
                        if !frame.is_masked_index(i) && v < sat {
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
            })
            .response
            .on_hover_text("Colour palette used to render pixel intensities");

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
        } // end active_panel == 1

        // -- Data browser --
        if let Some(ref mut db) = self.data_browser {
            if Self::accordion_header(ui, "Data browser", self.active_panel == 0) { self.active_panel = 0; }
            if self.active_panel == 0 {
                if let Some(path) = db.show(ui) {
                    panel_action = Some(path);
                }
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

        panel_action
    }

    fn show_viewport(&mut self, ctx: &Context, ui: &mut Ui) {
        let total_rect = ui.available_rect_before_wrap();

        // Reserve the bottom quarter for the Dozor quality chart when data is present and not collapsed.
        let chart_visible = self.dozor_data.is_some() && !self.dozor_collapsed;
        let available = if chart_visible {
            total_rect.with_max_y(total_rect.min.y + total_rect.height() * 0.75)
        } else {
            total_rect
        };

        self.last_viewport_rect = available;
        let response = ui.allocate_rect(available, egui::Sense::click_and_drag());
        response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Image, true, "Detector image viewport"));

        // Allocate the chart area now so egui accounts for it in the layout.
        let chart_response = if chart_visible {
            let crect = total_rect.with_min_y(available.max.y);
            let resp = ui.allocate_rect(crect, egui::Sense::click());
            resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Other, true, "Dozor quality chart"));
            Some(resp)
        } else {
            None
        };

        let Some(ref frame) = self.frame.clone() else {
            if !self.splash_loaded {
                self.splash_loaded = true;
                if let Some(ref dir) = self.splash_folder.clone() {
                    if let Some(path) = Self::pick_random_png(dir) {
                        match std::fs::read(&path) {
                            Ok(bytes) => match Self::decode_splash_png(&bytes) {
                                Ok(image) => {
                                    self.splash_texture = Some(ctx.load_texture(
                                        "splash",
                                        image,
                                        egui::TextureOptions::LINEAR,
                                    ));
                                }
                                Err(e) => eprintln!("splash: cannot decode {}: {e:#}", path.display()),
                            },
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

        // Hold F and drag with the left button: adjust the Foreground (vmax) setting.
        // Dragging right brightens the image (lower vmax), left darkens it.
        let contrast_drag = ctx.input(|i| i.key_down(egui::Key::F))
            && response.dragged_by(egui::PointerButton::Primary);
        if contrast_drag {
            const CONTRAST_DRAG_SPEED: f32 = 0.005; // log-units of vmax per pixel
            let dx = response.drag_delta().x;
            if dx != 0.0 {
                let max = self.effective_saturation() as f32;
                let min = self.contrast.vmin + AUTO_CONTRAST_MIN_SPAN;
                self.contrast.auto = false;
                self.contrast.vmax = (self.contrast.vmax * (-dx * CONTRAST_DRAG_SPEED).exp())
                    .clamp(min, max.max(min));
            }
        }

        // Handle pan + zoom input.
        let view_changed = viewport::handle_input(&mut self.view, &response, Some(frame), self.zoom_speed, !contrast_drag);
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

        // Right-button drag: start or update the line profile.
        if response.drag_started_by(egui::PointerButton::Secondary) {
            if let Some(pos) = response.interact_pointer_pos() {
                let img = self.view.screen_to_image(pos, available.min);
                self.line_profile_start = Some(img);
                self.line_profile_end = Some(img);
                self.line_profile_data.clear();
            }
        }
        if response.dragged_by(egui::PointerButton::Secondary) {
            if let Some(pos) = response.interact_pointer_pos() {
                let img = self.view.screen_to_image(pos, available.min);
                self.line_profile_end = Some(img);
                if let Some(start) = self.line_profile_start {
                    let profile = line_profile::sample(frame, start, img, self.line_profile_width);
                    self.line_profile_data = profile;
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
                    self.tone_params(),
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
        self.draw_series_name_overlay(ui, available, frame);

        // Fullscreen hint in the top-right corner.
        if ctx.input(|i| i.viewport().fullscreen.unwrap_or(false)) {
            painter.text(
                egui::pos2(available.right() - 8.0, available.top() + 8.0),
                egui::Align2::RIGHT_TOP,
                "Press F11 to exit fullscreen",
                egui::FontId::proportional(12.0),
                egui::Color32::from_rgba_unmultiplied(255, 255, 255, 160),
            );
        }

        // Hover tooltip balloon: pixel coords, value, and resolution near the cursor.
        // tooltip_rect is saved so the loupe can avoid overlapping it.
        let mut tooltip_rect: Option<egui::Rect> = None;
        if let Some(hover) = response.hover_pos() {
            let img_pos = self.view.screen_to_image(hover, available.min);
            let ix = img_pos.x as i64;
            let iy = img_pos.y as i64;
            if ix >= 0 && iy >= 0 && ix < frame.width as i64 && iy < frame.height as i64 {
                let pixel_index = (iy as u32 * frame.width + ix as u32) as usize;
                if !frame.is_masked_index(pixel_index) {
                    let value = frame.pixels[pixel_index];
                    let resolution = viewport::pixel_to_resolution(ix as f64, iy as f64, frame);

                    let line1 = format!("x={ix}  y={iy}  ={value}");
                    let line2 = resolution.map(|d| format!("d = {d:.2} Å"));

                    let font = egui::FontId::monospace(12.0);
                    let color = egui::Color32::WHITE;
                    let padding = egui::vec2(6.0, 4.0);
                    let line_gap = 2.0;

                    let g1 = painter.layout_no_wrap(line1, font.clone(), color);
                    let g2 = line2.as_deref().map(|s| painter.layout_no_wrap(s.to_string(), font.clone(), color));

                    let text_w = g2.as_ref().map_or(g1.size().x, |g| g1.size().x.max(g.size().x));
                    let text_h = g1.size().y + g2.as_ref().map_or(0.0, |g| line_gap + g.size().y);
                    let box_size = egui::vec2(text_w + padding.x * 2.0, text_h + padding.y * 2.0);

                    // Position at cursor + offset, clamped so the box stays inside the viewport.
                    let offset = self.overlays.hover_tooltip_offset;
                    let origin = egui::pos2(
                        (hover.x + offset.x).min(available.right()  - box_size.x - 2.0),
                        (hover.y + offset.y).min(available.bottom() - box_size.y - 2.0),
                    );
                    let bg_rect = egui::Rect::from_min_size(origin, box_size);
                    tooltip_rect = Some(bg_rect);

                    painter.rect_filled(bg_rect, 3.0, egui::Color32::from_rgba_unmultiplied(0, 0, 0, 180));
                    let text_origin = origin + padding;
                    let g1_h = g1.size().y;
                    painter.galley(text_origin, g1, color);
                    if let Some(g) = g2 {
                        painter.galley(text_origin + egui::vec2(0.0, g1_h + line_gap), g, color);
                    }
                }
            }
        }

        // Loupe (magnifying glass): hold Z to see a zoomed region around the cursor.
        let z_held = ctx.input(|i| i.key_down(egui::Key::Z));
        if z_held {
            if let Some(hover) = response.hover_pos() {
                let img_pos = self.view.screen_to_image(hover, available.min);
                let ix = img_pos.x;
                let iy = img_pos.y;
                if ix >= 0.0 && iy >= 0.0 && ix < frame.width as f32 && iy < frame.height as f32 {
                    let loupe_radius = self.overlays.loupe_radius as i64;
                    const LOUPE_PX: f32 = 200.0;

                    let fw = frame.width as f32;
                    let fh = frame.height as f32;
                    let ix_int = ix.floor() as i64;
                    let iy_int = iy.floor() as i64;

                    // UV snapped to integer pixel boundaries for clean cell alignment.
                    let src_x0 = (ix_int - loupe_radius).max(0);
                    let src_y0 = (iy_int - loupe_radius).max(0);
                    let src_x1 = (ix_int + loupe_radius).min(frame.width as i64);
                    let src_y1 = (iy_int + loupe_radius).min(frame.height as i64);
                    let uv = egui::Rect::from_min_max(
                        egui::pos2(src_x0 as f32 / fw, src_y0 as f32 / fh),
                        egui::pos2(src_x1 as f32 / fw, src_y1 as f32 / fh),
                    );

                    // Place the loupe in the first quadrant around the cursor that
                    // fits inside the viewport and does not overlap the tooltip.
                    let loupe_size = egui::vec2(LOUPE_PX, LOUPE_PX);
                    let m = 10.0_f32;
                    let offsets = [
                        egui::vec2(m, m),                           // lower-right
                        egui::vec2(m, -LOUPE_PX - m),              // upper-right
                        egui::vec2(-LOUPE_PX - m, m),              // lower-left
                        egui::vec2(-LOUPE_PX - m, -LOUPE_PX - m), // upper-left
                    ];
                    let loupe_rect = offsets.iter()
                        .map(|&off| {
                            let origin = egui::pos2(
                                (hover.x + off.x).clamp(available.left() + 4.0, available.right()  - LOUPE_PX - 4.0),
                                (hover.y + off.y).clamp(available.top()  + 4.0, available.bottom() - LOUPE_PX - 4.0),
                            );
                            egui::Rect::from_min_size(origin, loupe_size)
                        })
                        .find(|r| tooltip_rect.map_or(true, |tr| !r.intersects(tr)))
                        .unwrap_or_else(|| {
                            let origin = egui::pos2(
                                (hover.x + m).clamp(available.left() + 4.0, available.right()  - LOUPE_PX - 4.0),
                                (hover.y + m).clamp(available.top()  + 4.0, available.bottom() - LOUPE_PX - 4.0),
                            );
                            egui::Rect::from_min_size(origin, loupe_size)
                        });
                    let loupe_origin = loupe_rect.min;

                    painter.rect_filled(loupe_rect, 2.0, egui::Color32::from_rgba_unmultiplied(0, 0, 0, 220));
                    painter.image(texture_id, loupe_rect, uv, egui::Color32::WHITE);
                    painter.rect_stroke(loupe_rect, 2.0, egui::Stroke::new(1.5, egui::Color32::WHITE), egui::StrokeKind::Outside);

                    // Pixel value labels — one per source pixel, centered in its cell.
                    // Label colour is chosen for contrast against the rendered pixel colour.
                    let uv_w = uv.width();
                    let uv_h = uv.height();
                    let label_font = egui::FontId::monospace(7.0);
                    let params = ToneMapParams { saturation: frame.saturation_value, ..self.tone_params() };
                    for spy in src_y0..src_y1 {
                        for spx in src_x0..src_x1 {
                            let pixel_index = (spy as u32 * frame.width + spx as u32) as usize;
                            if frame.is_masked_index(pixel_index) {
                                continue;
                            }
                            let value = frame.pixels[pixel_index];
                            let screen_x = loupe_origin.x + ((spx as f32 + 0.5) / fw - uv.min.x) / uv_w * LOUPE_PX;
                            let screen_y = loupe_origin.y + ((spy as f32 + 0.5) / fh - uv.min.y) / uv_h * LOUPE_PX;
                            let [r, g, b] = crate::image_render::pixel_to_rgb(value, params);
                            let lum = 0.299 * r as f32 + 0.587 * g as f32 + 0.114 * b as f32;
                            let label_color = if lum > 140.0 { egui::Color32::BLACK } else { egui::Color32::WHITE };
                            painter.text(
                                egui::pos2(screen_x, screen_y),
                                egui::Align2::CENTER_CENTER,
                                value.to_string(),
                                label_font.clone(),
                                label_color,
                            );
                        }
                    }

                    // Crosshair marking the cursor pixel.
                    let ch_x = loupe_origin.x + ((ix_int as f32 + 0.5) / fw - uv.min.x) / uv_w * LOUPE_PX;
                    let ch_y = loupe_origin.y + ((iy_int as f32 + 0.5) / fh - uv.min.y) / uv_h * LOUPE_PX;
                    let cell = LOUPE_PX / (uv_w * fw);
                    let arm = cell * 0.4;
                    let ch_stroke = egui::Stroke::new(1.0, egui::Color32::from_rgba_unmultiplied(255, 80, 80, 220));
                    let c = egui::pos2(ch_x, ch_y);
                    painter.line_segment([egui::pos2(c.x - arm, c.y), egui::pos2(c.x + arm, c.y)], ch_stroke);
                    painter.line_segment([egui::pos2(c.x, c.y - arm), egui::pos2(c.x, c.y + arm)], ch_stroke);
                }
            }
        }

        // Line profile overlay: draw the line and integration band on the viewport.
        if let (Some(start), Some(end)) = (self.line_profile_start, self.line_profile_end) {
            let ss = self.view.image_to_screen(start, available.min);
            let se = self.view.image_to_screen(end, available.min);
            let dx = se.x - ss.x;
            let dy = se.y - ss.y;
            let len = (dx * dx + dy * dy).sqrt();
            if len > 0.5 {
                let nx = -dy / len;
                let ny = dx / len;
                let half_w = self.line_profile_width as f32 * self.view.zoom * 0.5;
                let amber = egui::Color32::from_rgba_unmultiplied(255, 200, 0, 220);
                let amber_fill = egui::Color32::from_rgba_unmultiplied(255, 200, 0, 45);
                let amber_edge = egui::Color32::from_rgba_unmultiplied(255, 200, 0, 150);

                if self.line_profile_width > 1 {
                    let pts = vec![
                        egui::pos2(ss.x + nx * half_w, ss.y + ny * half_w),
                        egui::pos2(ss.x - nx * half_w, ss.y - ny * half_w),
                        egui::pos2(se.x - nx * half_w, se.y - ny * half_w),
                        egui::pos2(se.x + nx * half_w, se.y + ny * half_w),
                    ];
                    painter.add(egui::Shape::convex_polygon(pts, amber_fill, egui::Stroke::new(0.8, amber_edge)));
                }

                painter.line_segment([ss, se], egui::Stroke::new(1.5, amber));
                painter.circle_filled(ss, 3.5, amber);
                painter.circle_filled(se, 3.5, amber);

                // Distance label near the end point.
                let dist_px = ((end.x - start.x).powi(2) + (end.y - start.y).powi(2)).sqrt();
                let label = format!("{dist_px:.0} px");
                let lpos = egui::pos2(se.x + 6.0, se.y - 12.0);
                painter.text(lpos, egui::Align2::LEFT_BOTTOM, label,
                    egui::FontId::proportional(11.0), amber);
            }
        }

        // Dozor toggle button — shown whenever dozor data is loaded.
        if self.dozor_data.is_some() {
            let label = if self.dozor_collapsed { "▲ Dozor" } else { "▼ Dozor" };
            let btn_w = 58.0_f32;
            let btn_h = 15.0_f32;
            let btn_rect = egui::Rect::from_min_size(
                egui::pos2(available.right() - btn_w - 6.0, available.bottom() - btn_h - 4.0),
                egui::vec2(btn_w, btn_h),
            );
            let toggle_resp = ui.allocate_rect(btn_rect, egui::Sense::click());
            ui.painter().rect_filled(
                btn_rect,
                3.0,
                egui::Color32::from_rgba_unmultiplied(0, 0, 0, 170),
            );
            ui.painter().text(
                btn_rect.center(),
                egui::Align2::CENTER_CENTER,
                label,
                egui::FontId::proportional(11.0),
                egui::Color32::WHITE,
            );
            if toggle_resp.clicked() {
                self.dozor_collapsed = !self.dozor_collapsed;
            }
        }

        // Dozor quality chart in the bottom quarter.
        let mut chart_clicked_number: Option<usize> = None;
        if let (Some(data), Some(cresp)) = (&self.dozor_data, chart_response) {
            let crect = total_rect.with_min_y(available.max.y);
            crate::dozor::draw_chart(ui.painter(), crect, data, self.hdf5_frame_index);
            let dozor_frame_at = |pos: egui::Pos2| {
                let first_number = data.frames.first()?.number as f32;
                let last_number = data.frames.last()?.number as f32;
                let number_span = (last_number - first_number).max(1.0);
                let t = ((pos.x - crect.left()) / crect.width()).clamp(0.0, 1.0);
                let number = first_number + t * number_span;
                data.frames.iter().min_by(|a, b| {
                    let da = (a.number as f32 - number).abs();
                    let db = (b.number as f32 - number).abs();
                    da.partial_cmp(&db).unwrap_or(std::cmp::Ordering::Equal)
                })
            };

            // Click to navigate to frame.
            if cresp.clicked() {
                if let Some(cpos) = cresp.interact_pointer_pos() {
                    chart_clicked_number = dozor_frame_at(cpos).map(|f| f.number as usize);
                }
            }

            // Hover tooltip (consumes cresp, must be last).
            if let Some(hpos) = cresp.hover_pos() {
                if let Some(f) = dozor_frame_at(hpos) {
                    cresp.on_hover_text(format!(
                        "Frame {}\nScore: {:.1}\nSpots: {}\nRes: {:.3} Å",
                        f.number, f.score, f.spots as u32, f.resolution
                    ));
                }
            }
        }
        // Apply chart navigation outside the dozor_data borrow.
        if let Some(number) = chart_clicked_number {
            self.hdf5_frame_index = number;
            self.load_hdf5_grouped(number);
        }
    }

    fn show_line_profile_window(&mut self, ctx: &Context) {
        if self.line_profile_data.is_empty() {
            return;
        }
        let data = self.line_profile_data.clone();
        let mut open = true;
        egui::Window::new("Line Profile")
            .resizable(true)
            .default_size([480.0, 180.0])
            .open(&mut open)
            .show(ctx, |ui| {
                let size = ui.available_size();
                let (resp, painter) = ui.allocate_painter(size, egui::Sense::hover());
                let peaks = match (self.frame.as_deref(), self.line_profile_start, self.line_profile_end) {
                    (Some(frame), Some(start), Some(end)) => {
                        line_profile::peaks_with_resolution(frame, start, end, &data)
                    }
                    _ => Vec::new(),
                };
                Self::draw_line_profile(&painter, resp.rect, &data, &peaks);
            });
        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            open = false;
        }
        if !open {
            self.line_profile_data.clear();
            self.line_profile_start = None;
            self.line_profile_end = None;
        }
    }

    fn draw_line_profile(painter: &egui::Painter, rect: egui::Rect, data: &[f32], peaks: &[LineProfilePeak]) {
        let n = data.len();
        if n < 2 { return; }

        painter.rect_filled(rect, 0.0, egui::Color32::from_rgba_unmultiplied(0, 0, 0, 215));

        let pad = egui::vec2(36.0, 26.0);
        let plot = egui::Rect::from_min_max(
            rect.min + pad,
            rect.max - egui::vec2(8.0, 18.0),
        );
        if plot.width() < 8.0 || plot.height() < 8.0 { return; }

        let min_v = data.iter().cloned().fold(f32::INFINITY, f32::min);
        let max_v = data.iter().cloned().fold(f32::NEG_INFINITY, f32::max);
        let range = (max_v - min_v).max(1.0);

        let x_of = |i: usize| plot.left() + (i as f32 / (n - 1) as f32) * plot.width();
        let y_of = |v: f32| plot.bottom() - ((v - min_v) / range) * plot.height();

        // Axes
        let axis_color = egui::Color32::from_rgba_unmultiplied(180, 180, 180, 200);
        let axis_stroke = egui::Stroke::new(1.0, axis_color);
        painter.line_segment([plot.left_bottom(), plot.right_bottom()], axis_stroke);
        painter.line_segment([plot.left_bottom(), plot.left_top()], axis_stroke);

        // Y-axis labels
        let font = egui::FontId::monospace(9.0);
        let label_color = egui::Color32::from_rgba_unmultiplied(180, 180, 180, 220);
        for (val, label) in [(max_v, format!("{max_v:.0}")), (min_v, format!("{min_v:.0}"))] {
            painter.text(
                egui::pos2(plot.left() - 2.0, y_of(val)),
                egui::Align2::RIGHT_CENTER,
                label,
                font.clone(),
                label_color,
            );
        }

        // X-axis labels (start and end distance)
        painter.text(
            egui::pos2(plot.left(), plot.bottom() + 2.0),
            egui::Align2::LEFT_TOP,
            "0",
            font.clone(),
            label_color,
        );
        painter.text(
            egui::pos2(plot.right(), plot.bottom() + 2.0),
            egui::Align2::RIGHT_TOP,
            format!("{} px", n - 1),
            font.clone(),
            label_color,
        );

        // Profile line
        let pts: Vec<egui::Pos2> = (0..n).map(|i| egui::pos2(x_of(i), y_of(data[i]))).collect();
        painter.add(egui::Shape::line(pts, egui::Stroke::new(1.5, egui::Color32::from_rgb(100, 210, 255))));

        if peaks.is_empty() {
            return;
        }

        let peak_color = egui::Color32::from_rgb(255, 200, 50);
        let gap_color = egui::Color32::from_rgb(230, 230, 230);
        let marker_stroke = egui::Stroke::new(1.0, peak_color);
        for peak in peaks {
            let x = x_of(peak.index);
            painter.line_segment([egui::pos2(x, plot.top()), egui::pos2(x, plot.bottom())], marker_stroke);
            painter.circle_filled(egui::pos2(x, y_of(data[peak.index])), 3.0, peak_color);
        }

        for pair in peaks.windows(2) {
            let a = &pair[0];
            let b = &pair[1];
            let (Some(da), Some(db)) = (a.d_spacing, b.d_spacing) else {
                continue;
            };
            let x0 = x_of(a.index);
            let x1 = x_of(b.index);
            if x1 - x0 < 28.0 {
                continue;
            }
            let y = plot.top() + 10.0;
            let stroke = egui::Stroke::new(1.0, gap_color);
            painter.line_segment([egui::pos2(x0, y), egui::pos2(x1, y)], stroke);
            painter.line_segment([egui::pos2(x0, y - 3.0), egui::pos2(x0, y + 3.0)], stroke);
            painter.line_segment([egui::pos2(x1, y - 3.0), egui::pos2(x1, y + 3.0)], stroke);
            painter.text(
                egui::pos2((x0 + x1) * 0.5, y - 3.0),
                egui::Align2::CENTER_BOTTOM,
                format!("Δd {:.3} Å", (db - da).abs()),
                egui::FontId::monospace(9.0),
                gap_color,
            );
        }
    }

    fn draw_series_name_overlay(&self, ui: &Ui, viewport: egui::Rect, frame: &Frame) {
        let Some(name_pattern) = frame.metadata.name_pattern.as_deref() else {
            return;
        };
        let full_label = match frame.metadata.data_collection_date.as_deref() {
            Some(date) => format!("Last dataset on {}: {}", date, name_pattern),
            None => name_pattern.to_string(),
        };
        let max_text_width = (viewport.width() - 44.0).max(0.0);
        let max_chars = (max_text_width / 7.0).floor().max(8.0) as usize;
        let label = crate::text::elide_middle(&full_label, max_chars);

        let font_id = egui::FontId::proportional(13.0);
        let galley = ui.painter().layout_no_wrap(
            label,
            font_id,
            egui::Color32::WHITE,
        );
        let padding = egui::vec2(10.0, 5.0);
        let rect_size = egui::vec2(
            galley.size().x + padding.x * 2.0,
            galley.size().y + padding.y * 2.0,
        );
        let rect = egui::Rect::from_min_size(
            egui::pos2(viewport.center().x - rect_size.x * 0.5, viewport.top() + 8.0),
            rect_size,
        );
        let painter = ui.painter().with_clip_rect(viewport);
        painter.rect_filled(
            rect,
            4.0,
            egui::Color32::from_rgba_unmultiplied(0, 0, 0, 170),
        );
        let text_pos = egui::pos2(
            rect.center().x - galley.size().x.min(rect.width() - padding.x * 2.0) * 0.5,
            rect.center().y - galley.size().y * 0.5,
        );
        painter.galley(text_pos, galley, egui::Color32::WHITE);
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
        let save_png_shortcut = KeyboardShortcut::new(Modifiers::COMMAND, Key::S);
        let movie_shortcut = KeyboardShortcut::new(Modifiers::COMMAND, Key::P);
        let rings_shortcut = KeyboardShortcut::new(Modifiers::COMMAND, Key::R);

        // Movie mode never runs without an HDF5 series, nor against the live monitor
        // display — but a remote-started movie is allowed to keep playing while
        // connected, since on_demand_active means it's showing the on-demand HDF5
        // series rather than the live feed.
        if self.movie_playing && (self.hdf5_series.is_none() || (self.connected && !self.on_demand_active)) {
            self.movie_playing = false;
        }
        if ctx.input_mut(|i| i.consume_shortcut(&rings_shortcut)) {
            self.overlays.show_resolution_rings = !self.overlays.show_resolution_rings;
        }
        if ctx.input_mut(|i| i.consume_shortcut(&movie_shortcut)) {
            self.toggle_movie();
        }

        if ctx.input_mut(|i| i.consume_shortcut(&help_shortcut)) {
            self.show_help = !self.show_help;
        }

        if ctx.input_mut(|i| i.consume_shortcut(&panel_shortcut)) {
            self.show_panel = !self.show_panel;
        }

        if ctx.input_mut(|i| i.consume_shortcut(&fullscreen_shortcut)) {
            let is_fullscreen = ctx.input(|i| i.viewport().fullscreen.unwrap_or(false));
            if is_fullscreen {
                // Exiting fullscreen: ensure the panel is visible.
                self.show_panel = true;
            }
            ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(!is_fullscreen));
        }

        if self.show_help {
            self.show_help_window(ctx);
        }

        self.show_actions_window(ctx);
        self.show_line_profile_window(ctx);

        if ctx.input_mut(|i| i.consume_shortcut(&goto_shortcut)) {
            self.show_help = false;
            self.show_goto_frame = true;
            self.goto_frame_needs_focus = true;
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

        if ctx.input_mut(|i| i.consume_shortcut(&save_png_shortcut)) {
            println!("saving PNG...");

            self.save_png();
        }

        // egui-winit turns Ctrl+C (with or without Shift) into Event::Copy and never
        // emits the key event, so a KeyboardShortcut for it can't match. Use the event
        // instead, unless a text field is focused and wants it for its own copy.
        if !ctx.wants_keyboard_input() {
            let copy_requested = ctx.input_mut(|i| {
                let before = i.events.len();
                i.events.retain(|e| !matches!(e, egui::Event::Copy));
                i.events.len() != before
            });
            if copy_requested {
                self.copy_image(ctx);
            }
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

            if self.movie_playing {
                let interval = std::time::Duration::from_secs_f32(1.0 / self.movie_fps.max(0.5));
                let elapsed = self.movie_last_advance.elapsed();
                if elapsed >= interval {
                    if self.hdf5_frame_index + grouping < total {
                        self.hdf5_frame_index += grouping;
                        self.movie_last_advance = std::time::Instant::now();
                    } else {
                        self.movie_playing = false; // reached the end
                    }
                }
                if self.movie_playing {
                    ctx.request_repaint_after(interval.saturating_sub(self.movie_last_advance.elapsed()));
                }
            }

            if self.hdf5_frame_index != old_index {
                self.load_hdf5_grouped(self.hdf5_frame_index);
            }
        }

        self.poll_hdf5_loader();

        // Poll monitor prefetcher and upload any completed textures.
        if self.monitor_prefetcher.poll(ctx) {
            ctx.request_repaint();
        }

        // Detect pause-expiry transition before anything else touches display state.
        let currently_paused = self.interaction_paused();
        let just_resumed = self.was_paused && !currently_paused;
        self.was_paused = currently_paused;

        // Invalidate monitor prefetcher cache when contrast settings change.
        let cur_contrast = self.tone_params();
        // While the settings are changing (slider drag) only invalidate: the frame on
        // screen is re-rendered synchronously, and the rest of the batch is
        // re-queued once the settings have been stable for a repaint.
        if !self.monitor_frames.is_empty() && !self.on_demand_active {
            if Some(cur_contrast) != self.monitor_contrast {
                self.monitor_prefetcher.invalidate();
                self.monitor_prefetch_pending = true;
                ctx.request_repaint_after(std::time::Duration::from_millis(150));
            } else if self.monitor_prefetch_pending {
                self.monitor_prefetch_pending = false;
                self.monitor_prefetcher.submit_batch_skipping(
                    &self.monitor_frames,
                    Some(self.monitor_frame_index),
                    false,
                    cur_contrast,
                );
            }
        }
        self.monitor_contrast = Some(cur_contrast);

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
                self.tone_params(),
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

        // Receive remote commands (load HDF5 file + frame, or start/stop movie,
        // from an external client), acknowledging each one back over its socket.
        while let Ok(req) = self.remote_rx.try_recv() {
            let result = self.handle_remote_cmd(req.cmd);
            let _ = req.ack.send(result);
            ctx.request_repaint();
        }
        if let Some(rx) = &mut self.commands_file_rx {
            let mut commands = Vec::new();
            while let Ok(cmd) = rx.try_recv() {
                commands.push(cmd);
            }
            for cmd in commands {
                let _ = self.handle_remote_cmd(cmd);
                ctx.request_repaint();
            }
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

        // Poll data browser background loads; request repaint while loading.
        if let Some(ref mut db) = self.data_browser {
            if db.poll() { ctx.request_repaint(); }
            if db.is_loading() {
                ctx.request_repaint_after(std::time::Duration::from_millis(100));
            }
        }

        let mut browser_file: Option<std::path::PathBuf> = None;
        SidePanel::left("left_panel").resizable(true).default_width(240.0).show_animated(ctx, self.show_panel, |ui| {
            ScrollArea::vertical().show(ui, |ui| {
                browser_file = self.show_left_panel(ui);
            });
        });
        if let Some(path) = browser_file {
            self.disconnect();
            if let Some(parent) = path.parent() {
                self.last_location = Some(parent.to_path_buf());
                self.save_last_location();
            }
            if let Err(e) = self.load_hdf5_master(&path) {
                eprintln!("Data browser: failed to open {}: {e}", path.display());
            }
        }

        CentralPanel::default().show(ctx, |ui| {
            self.show_viewport(ctx, ui);
        });
    }
}
