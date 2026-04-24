use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::mpsc;

use egui::{ColorImage, Context, TextureHandle, TextureOptions};

use crate::frame::Frame;
use crate::image_render::Colormap;

/// How many frames on either side of the current frame to keep cached in VRAM.
const CACHE_RADIUS: usize = 3;

struct Request {
    generation: u64,
    index: usize,
    vmin: f32,
    vmax: f32,
    colormap: Colormap,
}

struct Ready {
    generation: u64,
    index: usize,
    frame: Frame,
    image: ColorImage,
}

pub struct CachedFrame {
    pub frame: Frame,
    pub texture: TextureHandle,
}

/// Prefetches HDF5 frames into VRAM in a background thread.
///
/// The background thread owns a separate HDF5 file handle so it never
/// blocks the main thread. Completed frames are uploaded to the GPU
/// by `poll()` on the next repaint.
pub struct HDF5Prefetcher {
    cached: HashMap<usize, CachedFrame>,
    in_flight: HashSet<usize>,
    /// Incremented on contrast change; stale results are discarded.
    generation: u64,
    req_tx: mpsc::SyncSender<Request>,
    res_rx: mpsc::Receiver<Ready>,
}

impl HDF5Prefetcher {
    pub fn new(master_path: PathBuf, saturation: u16) -> Self {
        let (req_tx, req_rx) = mpsc::sync_channel(32);
        let (res_tx, res_rx) = mpsc::sync_channel(8);
        std::thread::spawn(move || reader_thread(master_path, saturation, req_rx, res_tx));
        Self { cached: HashMap::new(), in_flight: HashSet::new(), generation: 0, req_tx, res_rx }
    }

    pub fn get(&self, index: usize) -> Option<&CachedFrame> {
        self.cached.get(&index)
    }

    /// Request background load+tone-map for `index` if not already cached or in-flight.
    pub fn request(&mut self, index: usize, vmin: f32, vmax: f32, colormap: Colormap) {
        if self.cached.contains_key(&index) || self.in_flight.contains(&index) {
            return;
        }
        let req = Request { generation: self.generation, index, vmin, vmax, colormap };
        if self.req_tx.try_send(req).is_ok() {
            self.in_flight.insert(index);
        }
    }

    /// Drain completed results and upload textures to the GPU.
    /// Returns `true` if any new textures became available.
    pub fn poll(&mut self, ctx: &Context) -> bool {
        let mut any = false;
        while let Ok(ready) = self.res_rx.try_recv() {
            self.in_flight.remove(&ready.index);
            if ready.generation != self.generation {
                continue; // stale result from before a contrast change
            }
            let texture = ctx.load_texture(
                format!("hdf5_{}", ready.index),
                ready.image,
                TextureOptions::NEAREST,
            );
            self.cached.insert(ready.index, CachedFrame { frame: ready.frame, texture });
            any = true;
        }
        any
    }

    /// Drop textures far from `current` to cap VRAM usage.
    pub fn evict_distant(&mut self, current: usize) {
        self.cached.retain(|&idx, _| idx.abs_diff(current) <= CACHE_RADIUS);
        self.in_flight.retain(|&idx| idx.abs_diff(current) <= CACHE_RADIUS);
    }

    /// Invalidate all cached textures after a contrast settings change.
    pub fn invalidate(&mut self) {
        self.cached.clear();
        self.in_flight.clear();
        self.generation += 1;
    }
}

fn reader_thread(
    path: PathBuf,
    saturation: u16,
    rx: mpsc::Receiver<Request>,
    tx: mpsc::SyncSender<Ready>,
) {
    let series = match crate::hdf5_loader::Hdf5Series::open(&path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Prefetch thread: failed to open {}: {e}", path.display());
            return;
        }
    };

    for req in rx {
        match series.load_frame(req.index) {
            Ok(frame) => {
                let rgba = crate::image_render::tone_map(
                    &frame.pixels,
                    frame.width,
                    frame.height,
                    req.vmin,
                    req.vmax,
                    saturation,
                    req.colormap,
                );
                let image = ColorImage::from_rgba_unmultiplied(
                    [frame.width as usize, frame.height as usize],
                    &rgba,
                );
                let ready = Ready { generation: req.generation, index: req.index, frame, image };
                if tx.send(ready).is_err() {
                    break;
                }
            }
            Err(e) => eprintln!("Prefetch: frame {}: {e}", req.index),
        }
    }
}
