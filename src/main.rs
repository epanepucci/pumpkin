mod app;
mod frame;
mod hdf5_loader;
mod image_render;
mod monitor;
mod tiff_loader;
mod viewport;

use app::PumpkinApp;
use clap::Parser;

#[derive(Parser)]
#[command(name = "pumpkin", about = "X-ray diffraction viewer")]
struct Args {
    /// Base URL of the DCU (detector control unit)
    #[arg(long, default_value = "http://localhost")]
    dcu_url: String,

    /// How often to poll the monitor buffer list for new images (milliseconds)
    #[arg(long, default_value_t = 500)]
    poll_period_ms: u64,
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse();

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

    let dcu_url = args.dcu_url;
    let poll_period_ms = args.poll_period_ms;
    eframe::run_native(
        "pumpkin",
        native_options,
        Box::new(|cc| Ok(Box::new(PumpkinApp::new(cc, dcu_url, poll_period_ms)))),
    )
    .map_err(|e| anyhow::anyhow!("eframe error: {e}"))?;

    Ok(())
}
