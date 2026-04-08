mod app;
mod frame;
mod hdf5_loader;
mod image_render;
mod monitor;
mod tiff_loader;
mod viewport;

use app::PumpkinApp;

fn main() -> anyhow::Result<()> {
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

    eframe::run_native("pumpkin", native_options, Box::new(|cc| Ok(Box::new(PumpkinApp::new(cc)))))
        .map_err(|e| anyhow::anyhow!("eframe error: {e}"))?;

    Ok(())
}
