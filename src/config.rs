/// Pumpkin configuration file, loaded from `~/.config/pumpkin/config.toml`
/// or from the path given by `--config`.
///
/// All fields are optional — missing fields fall back to the built-in defaults.
///
/// Example:
/// ```toml
/// dcu_url       = "http://192.168.1.100"
/// poll_period_ms = 200
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
    /// Connect to the detector automatically on startup.
    #[serde(alias = "auto-connect")]
    pub auto_connect: Option<bool>,
    /// Directory containing HDF5 filter plugins (e.g. bitshuffle). Overrides
    /// auto-discovery; still superseded by the HDF5_PLUGIN_PATH env var.
    #[serde(alias = "hdf5-plugin-path")]
    pub hdf5_plugin_path: Option<PathBuf>,
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
