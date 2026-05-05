mod app;
mod config;
mod png_export;
mod frame;
mod hdf5_loader;
mod hdf5_prefetch;
mod image_render;
mod monitor;
mod monitor_prefetch;
mod tiff_loader;
mod viewport;

use std::path::PathBuf;

use app::{ContrastState, PumpkinApp};
use clap::Parser;
use viewport::{OverlaySettings, ResolutionRing};

use crate::config::Config;
use crate::image_render::Colormap;

#[derive(Parser)]
#[command(name = "pumpkin", about = "X-ray diffraction viewer")]
struct Args {
    /// Base URL of the DCU; if given, connects automatically on startup.
    /// Overrides the config file value.
    #[arg(long)]
    dcu_url: Option<String>,

    /// Monitor poll interval in milliseconds. Overrides the config file value.
    #[arg(long)]
    poll_period_ms: Option<u64>,

    /// Path to a TOML config file.
    /// Defaults to $HOME/.config/pumpkin/config.toml if not given.
    #[arg(long)]
    config: Option<PathBuf>,
}

/// Ensure HDF5_PLUGIN_PATH points to an existing directory.
///
/// The bundled static HDF5 (hdf5-src) bakes in the Cargo build output dir as
/// its default plugin path.  That directory only exists during `cargo build`
/// and is missing at runtime, causing H5Dread to fail with "can't open
/// directory".  Setting HDF5_PLUGIN_PATH to any real directory suppresses
/// that error.  We prefer a directory that actually contains the bitshuffle
/// plugin (filter 32008, used by DECTRIS EIGER by default) so those files
/// open without extra configuration.
fn setup_hdf5_plugin_path(config_path: Option<&std::path::Path>) {
    if std::env::var("HDF5_PLUGIN_PATH").is_ok() {
        return; // already set by the user — respect it
    }

    let plugin_filenames = ["libH5Zbshuf.so", "libhdf5_bshuf.so", "libh5bshuf.so"];

    // Config-specified path takes priority over auto-discovery candidates.
    let mut candidates: Vec<String> = Vec::new();
    if let Some(p) = config_path {
        candidates.push(p.to_string_lossy().into_owned());
    }
    candidates.extend([
        "/usr/lib/x86_64-linux-gnu/hdf5/serial/plugins".to_string(),
        "/usr/lib/x86_64-linux-gnu/hdf5/plugins".to_string(),
        "/usr/lib64/hdf5/plugins".to_string(),
        "/usr/local/hdf5/lib/plugin".to_string(),
    ]);
    if let Ok(home) = std::env::var("HOME") {
        candidates.push(format!("{home}/.hdf5/lib/plugin"));
    }

    // Prefer a directory that has the bitshuffle plugin.
    for dir in &candidates {
        let dir_path = std::path::Path::new(dir.as_str());
        if plugin_filenames.iter().any(|name| dir_path.join(name).exists()) {
            // Safety: called before any threads are spawned.
            unsafe { std::env::set_var("HDF5_PLUGIN_PATH", dir) };
            eprintln!("HDF5_PLUGIN_PATH set to: {dir}");
            return;
        }
    }

    // No bitshuffle plugin found.  Still need to point HDF5 at a directory
    // that exists — otherwise the bundled HDF5's baked-in (missing) path
    // causes H5Dread to fail entirely.  Use ~/.hdf5/lib/plugin (HDF5's own
    // standard user plugin dir), creating it if necessary.
    let fallback = std::env::var("HOME")
        .map(|h| format!("{h}/.hdf5/lib/plugin"))
        .unwrap_or_else(|_| "/tmp".to_string());
    std::fs::create_dir_all(&fallback).ok();
    unsafe { std::env::set_var("HDF5_PLUGIN_PATH", &fallback) };
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    // Load config: explicit path → error if missing; default path → silent skip.
    let (cfg, cfg_path) = match args.config {
        Some(ref path) => (Config::load(path)?, Some(path.clone())),
        None => Config::load_default(),
    };

    if let Some(path) = &cfg_path {
        eprintln!("Loaded config from: {}", path.display());
    } else {
        eprintln!("No config file found, using defaults.");
    }

    // Must be called before any HDF5 operations and before threads are spawned.
    setup_hdf5_plugin_path(cfg.hdf5_plugin_path.as_deref());

    // CLI args take precedence over config file values.
    let dcu_url_from_cli = args.dcu_url.is_some();
    let dcu_url = args.dcu_url
        .or(cfg.dcu_url.clone())
        .unwrap_or_else(|| "http://localhost".to_string());

    let poll_period_ms = args.poll_period_ms
        .or(cfg.poll_period_ms)
        .unwrap_or(500);

    // auto_connect: CLI flag wins; then config file; then fall back to
    // "only if --dcu-url was given on the CLI".
    let auto_connect = cfg.auto_connect.unwrap_or(dcu_url_from_cli);

    // Build initial contrast state from config.
    let mut contrast = ContrastState::default();
    if let Some(ref name) = cfg.contrast.colormap {
        contrast.colormap = parse_colormap(name);
    }
    if let Some(a) = cfg.contrast.gamma_correction {
        contrast.gamma_correction = a;
    }

    // Build initial overlay settings from config.
    let mut overlays = OverlaySettings::default();
    if let Some(e) = cfg.resolution_rings.enabled {
        overlays.show_resolution_rings = e;
    }
    if let Some(c) = cfg.resolution_rings.color {
        eprintln!("Applying resolution_rings.color: {:?}", c);
        overlays.ring_color = egui::Color32::from_rgba_unmultiplied(c[0], c[1], c[2], c[3]);
    }
    if let Some(w) = cfg.resolution_rings.stroke_width {
        overlays.ring_stroke_width = w;
    }
    if let Some(s) = cfg.resolution_rings.font_scale {
        overlays.ring_font_scale = s;
    }
    if let Some(rings) = cfg.rings {
        overlays.resolution_rings = rings
            .into_iter()
            .map(|r| match r.label {
                Some(label) => ResolutionRing::with_label(r.resolution, label),
                None => ResolutionRing::new(r.resolution),
            })
            .collect();
    }
    if let Some(e) = cfg.beam_center.enabled {
        overlays.show_beam_center = e;
    }
    if let Some(c) = cfg.beam_center.color {
        eprintln!("Applying beam_center.color: {:?}", c);
        overlays.beam_center_color = egui::Color32::from_rgba_unmultiplied(c[0], c[1], c[2], c[3]);
    }
    if let Some(w) = cfg.beam_center.stroke_width {
        overlays.beam_center_stroke_width = w;
    }

    // Build a tokio runtime and run it in a background thread so that async
    // monitor polling coexists with the egui event loop on the main thread.
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    let _guard = rt.enter();

    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Pumpkin — X-ray diffraction viewer")
            .with_inner_size([1280.0, 900.0])
            .with_min_inner_size([640.0, 480.0]),
        renderer: eframe::Renderer::Wgpu,
        ..Default::default()
    };

    eframe::run_native(
        "pumpkin",
        native_options,
        Box::new(move |cc| {
            Ok(Box::new(PumpkinApp::new(cc, dcu_url, poll_period_ms, auto_connect, contrast, overlays)))
        }),
    )
    .map_err(|e| anyhow::anyhow!("eframe error: {e}"))?;

    Ok(())
}

fn parse_colormap(name: &str) -> Colormap {
    match name.to_lowercase().as_str() {
        "standard" => Colormap::Standard,
        "grayscale" | "greyscale" => Colormap::Grayscale,
        "inferno" => Colormap::Inferno,
        "rocket" => Colormap::Rocket,
        "heat" => Colormap::Heat,
        other => {
            eprintln!("Warning: unknown colormap '{other}', using Inferno");
            Colormap::Inferno
        }
    }
}
