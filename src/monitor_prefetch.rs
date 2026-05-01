use std::collections::{HashMap, HashSet};
use std::sync::{Arc, mpsc};

use egui::{ColorImage, Context, TextureHandle, TextureOptions};

use crate::frame::Frame;
use crate::image_render::{Colormap, RadialMode, compute_radial_profile};

struct Ready {
    generation: u64,
    index: usize,
    image: ColorImage,
}

/// Tone-maps monitor frames in the background and caches the resulting GPU textures.
///
/// Because monitor frames are already in RAM (fetched from HTTP), there is no
/// I/O step — only tone-mapping, which is farmed out to rayon.
pub struct MonitorPrefetcher {
    textures: HashMap<usize, TextureHandle>,
    in_flight: HashSet<usize>,
    /// Incremented on series change or contrast change; stale results are discarded.
    generation: u64,
    res_tx: mpsc::SyncSender<Ready>,
    res_rx: mpsc::Receiver<Ready>,
}

impl MonitorPrefetcher {
    pub fn new() -> Self {
        let (res_tx, res_rx) = mpsc::sync_channel(16);
        Self { textures: HashMap::new(), in_flight: HashSet::new(), generation: 0, res_tx, res_rx }
    }

    pub fn get(&self, index: usize) -> Option<&TextureHandle> {
        self.textures.get(&index)
    }

    /// Submit all frames in a new batch for background tone-mapping.
    /// Pass `new_series = true` when the series ID changed to clear the cache.
    pub fn submit_batch(
        &mut self,
        frames: &[Arc<Frame>],
        new_series: bool,
        vmin: f32,
        vmax: f32,
        gamma_correction: f32,
        saturation: u16,
        colormap: Colormap,
        radial_mode: RadialMode,
    ) {
        if new_series {
            self.textures.clear();
            self.in_flight.clear();
            self.generation += 1;
        }

        let generation = self.generation;
        for (idx, frame) in frames.iter().enumerate() {
            if self.textures.contains_key(&idx) || self.in_flight.contains(&idx) {
                continue;
            }
            let frame = frame.clone();
            let tx = self.res_tx.clone();
            rayon::spawn(move || {
                let (profile, cx, cy) = if radial_mode != RadialMode::None {
                    if let (Some(cx), Some(cy)) = (frame.metadata.beam_center_x, frame.metadata.beam_center_y) {
                        let p = compute_radial_profile(&frame.pixels, frame.width, frame.height, cx, cy, saturation);
                        (p, cx as f32, cy as f32)
                    } else {
                        (Vec::new(), 0.0f32, 0.0f32)
                    }
                } else {
                    (Vec::new(), 0.0f32, 0.0f32)
                };
                let rgba = crate::image_render::tone_map(
                    &frame.pixels,
                    frame.width,
                    frame.height,
                    vmin,
                    vmax,
                    gamma_correction,
                    saturation,
                    colormap,
                    radial_mode,
                    &profile,
                    cx,
                    cy,
                );
                let image = ColorImage::from_rgba_unmultiplied(
                    [frame.width as usize, frame.height as usize],
                    &rgba,
                );
                // try_send: never block a rayon thread waiting on a full channel.
                let _ = tx.try_send(Ready { generation, index: idx, image });
            });
            self.in_flight.insert(idx);
        }
    }

    /// Drain completed tone-maps and upload textures to the GPU.
    /// Returns `true` if any new textures became available.
    pub fn poll(&mut self, ctx: &Context) -> bool {
        let mut any = false;
        while let Ok(ready) = self.res_rx.try_recv() {
            self.in_flight.remove(&ready.index);
            if ready.generation != self.generation {
                continue; // stale result from a previous series or contrast setting
            }
            let handle = ctx.load_texture(
                format!("monitor_{}", ready.index),
                ready.image,
                TextureOptions::NEAREST,
            );
            self.textures.insert(ready.index, handle);
            any = true;
        }
        any
    }

    /// Invalidate all cached textures (e.g., on contrast change).
    pub fn invalidate(&mut self) {
        self.textures.clear();
        self.in_flight.clear();
        self.generation += 1;
    }
}
