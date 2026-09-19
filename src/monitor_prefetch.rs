use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, mpsc};

use egui::{ColorImage, Context, TextureHandle, TextureOptions};

use crate::frame::Frame;
use crate::image_render::ToneMapParams;

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
    /// Mirror of `generation` that queued rayon tasks check before starting,
    /// so tasks made stale by a newer contrast setting exit without work.
    live_generation: Arc<AtomicU64>,
    res_tx: mpsc::SyncSender<Ready>,
    res_rx: mpsc::Receiver<Ready>,
}

impl MonitorPrefetcher {
    pub fn new() -> Self {
        let (res_tx, res_rx) = mpsc::sync_channel(16);
        Self { textures: HashMap::new(), in_flight: HashSet::new(), generation: 0, live_generation: Arc::new(AtomicU64::new(0)), res_tx, res_rx }
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
        params: ToneMapParams,
    ) {
        self.submit_batch_skipping(frames, None, new_series, params);
    }

    /// Like `submit_batch`, but leaves frame `skip` alone (e.g. the frame on
    /// screen, which is rendered synchronously by the caller).
    pub fn submit_batch_skipping(
        &mut self,
        frames: &[Arc<Frame>],
        skip: Option<usize>,
        new_series: bool,
        params: ToneMapParams,
    ) {
        if new_series {
            self.textures.clear();
            self.in_flight.clear();
            self.generation += 1;
            self.live_generation.store(self.generation, Ordering::Relaxed);
        }

        let generation = self.generation;
        for (idx, frame) in frames.iter().enumerate() {
            if Some(idx) == skip || self.textures.contains_key(&idx) || self.in_flight.contains(&idx) {
                continue;
            }
            let frame = frame.clone();
            let tx = self.res_tx.clone();
            let live = self.live_generation.clone();
            rayon::spawn(move || {
                if live.load(Ordering::Relaxed) != generation {
                    return;
                }
                let image = crate::image_render::tone_map_image(
                    &frame.pixels,
                    frame.pixel_mask.as_deref(),
                    frame.width,
                    frame.height,
                    params,
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
        self.live_generation.store(self.generation, Ordering::Relaxed);
    }
}
