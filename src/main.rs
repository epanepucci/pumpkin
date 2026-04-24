mod app;
mod config;
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
use viewport::OverlaySettings;

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

fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    // Load config: explicit path → error if missing; default path → silent skip.
    let cfg = match args.config {
        Some(ref path) => Config::load(path)?,
        None => Config::load_default(),
    };

    // CLI args take precedence over config file values.
    let dcu_url_from_cli = args.dcu_url.is_some();
    let dcu_url = args.dcu_url
        .or(cfg.dcu_url)
        .unwrap_or_else(|| "http://localhost".to_string());

    let poll_period_ms = args.poll_period_ms
        .or(cfg.poll_period_ms)
        .unwrap_or(500);

    // Only auto-connect when --dcu-url was explicitly given on the CLI,
    // not when it came from the config file (user can click Connect themselves).
    let auto_connect = dcu_url_from_cli;

    // Build initial contrast state from config.
    let mut contrast = ContrastState::default();
    if let Some(ref name) = cfg.contrast.colormap {
        contrast.colormap = parse_colormap(name);
    }

    // Build initial overlay settings from config.
    let mut overlays = OverlaySettings::default();
    if let Some(c) = cfg.resolution_rings.color {
        overlays.ring_color = egui::Color32::from_rgba_unmultiplied(c[0], c[1], c[2], c[3]);
    }
    if let Some(w) = cfg.resolution_rings.stroke_width {
        overlays.ring_stroke_width = w;
    }
    if let Some(s) = cfg.resolution_rings.font_scale {
        overlays.ring_font_scale = s;
    }
    if let Some(c) = cfg.beam_center.color {
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
