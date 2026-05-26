/// Pumpkin configuration file, loaded from `~/.config/pumpkin/config.toml`
/// or from the path given by `--config`.
///
/// All fields are optional — missing fields fall back to the built-in defaults.
///
/// Example:
/// ```toml
/// dcu_url                  = "http://192.168.1.100"
/// poll_period_ms           = 200
/// unfocused_poll_period_ms = 2000   # slower poll rate when window is not focused
/// monitor_pause_ms         = 2000   # pause live updates for this many ms after zoom/pan
/// pause_if_idle_after      = 600    # pause monitoring after N seconds idle (0 = never)
///
/// [contrast]
/// colormap = "Inferno"   # Standard | Grayscale | Inferno | Rocket | Heat
///
/// [resolution_rings]
/// color        = [0, 200, 255, 200]   # [R, G, B, A]  0-255
/// stroke_width = 1.0
/// font_scale   = 1.0
///
/// [beam_center]
/// color        = [0, 200, 255, 200]
/// stroke_width = 1.5
/// ```
use std::path::{Path, PathBuf};

use serde::Deserialize;

#[derive(Deserialize, Default)]
#[serde(default)]
pub struct Config {
    pub dcu_url: Option<String>,
    pub poll_period_ms: Option<u64>,
    /// Poll period when the window is visible but not focused (ms). Default 2000.
    /// The monitor continues downloading frames at this slower rate so it stays
    /// up to date without hammering the detector when the user is doing something else.
    #[serde(alias = "unfocused-poll-period-ms")]
    pub unfocused_poll_period_ms: Option<u64>,
    /// How long to pause live frame updates after a user zoom/pan interaction (ms).
    /// Default is 2000 (2 seconds). Set to 0 to disable the pause.
    #[serde(alias = "monitor-pause-ms")]
    pub monitor_pause_ms: Option<u64>,
    /// Pause monitoring completely after this many seconds of inactivity (seconds).
    /// Default is 600 (10 minutes). Set to 0 to never pause on idle.
    #[serde(alias = "pause-if-idle-after")]
    pub pause_if_idle_after: Option<u64>,
    /// Connect to the detector automatically on startup.
    #[serde(alias = "auto-connect")]
    pub auto_connect: Option<bool>,
    /// Directory containing HDF5 filter plugins (e.g. bitshuffle). Overrides
    /// auto-discovery; still superseded by the HDF5_PLUGIN_PATH env var.
    #[serde(alias = "hdf5-plugin-path")]
    pub hdf5_plugin_path: Option<PathBuf>,
    /// Directory containing PNG images shown as splash art when no image is loaded.
    /// A random PNG from this folder is chosen each launch.
    #[serde(alias = "splash-folder")]
    pub splash_folder: Option<PathBuf>,
    /// Global UI scale factor (pixels per point). Default is 1.0.
    /// Increase for high-DPI displays or larger widgets; decrease to fit more on screen.
    #[serde(alias = "ui-scale")]
    pub ui_scale: Option<f32>,
    /// TCP port for the remote control interface. Default is 8100.
    /// Send newline-delimited JSON: {"file": "/path/to/master.h5", "frame": 42}
    #[serde(alias = "remote-port")]
    pub remote_port: Option<u16>,
    pub contrast: ContrastConfig,
    #[serde(alias = "resolution-rings")]
    pub resolution_rings: RingConfig,
    #[serde(alias = "beam-center")]
    pub beam_center: BeamCenterConfig,
    /// Explicit ring definitions. When present, supersedes the built-in defaults.
    /// Each entry requires `resolution` (d-spacing in Å) and optionally `label`.
    pub rings: Option<Vec<RingEntry>>,
}

/// One explicit resolution ring from the config file.
///
/// ```toml
/// [[rings]]
/// resolution = 3.5
/// label = "ice 3.5"
///
/// [[rings]]
/// resolution = 2.0
/// ```
#[derive(Deserialize)]
pub struct RingEntry {
    /// d-spacing in Ångströms.
    pub resolution: f64,
    /// Text drawn next to the ring. Defaults to "<resolution> Å".
    pub label: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
pub struct ContrastConfig {
    /// Colormap name: Standard | Grayscale | Inferno | Rocket | Heat
    pub colormap: Option<String>,
    #[serde(alias = "attenuation")]
    pub gamma_correction: Option<f32>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
pub struct RingConfig {
    pub enabled: Option<bool>,
    /// RGBA color as [R, G, B, A], each 0–255.
    pub color: Option<[u8; 4]>,
    #[serde(alias = "thickness")]
    pub stroke_width: Option<f32>,
    /// Multiplier applied to the ring-label font size (default 1.0).
    pub font_scale: Option<f32>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
pub struct BeamCenterConfig {
    pub enabled: Option<bool>,
    /// RGBA color as [R, G, B, A], each 0–255.
    pub color: Option<[u8; 4]>,
    #[serde(alias = "thickness")]
    pub stroke_width: Option<f32>,
}

impl Config {
    /// Load a config from an explicit path. Returns an error if the file cannot
    /// be read or parsed.
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| anyhow::anyhow!("Cannot read {}: {e}", path.display()))?;
        toml::from_str(&text)
            .map_err(|e| anyhow::anyhow!("Parse error in {}: {e}", path.display()))
    }

    /// Try the default location (`$HOME/.config/pumpkin/config.toml`).
    /// Returns `Config::default()` if the file does not exist; warns on parse errors.
    pub fn load_default() -> (Self, Option<PathBuf>) {
        if let Some(path) = default_config_path() {
            if path.exists() {
                match Self::load(&path) {
                    Ok(c) => return (c, Some(path)),
                    Err(e) => {
                        eprintln!("Warning: {e}");
                        return (Self::default(), Some(path));
                    }
                }
            }
        }
        (Self::default(), None)
    }
}

fn default_config_path() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|h| {
        PathBuf::from(h).join(".config").join("pumpkin").join("config.toml")
    })
}
