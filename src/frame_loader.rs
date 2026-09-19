use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, mpsc};

use crate::frame::Frame;
use crate::hdf5_loader::Hdf5Series;

/// Number of decoded frames kept for instant back/forward navigation.
const CACHE_CAPACITY: usize = 4;

/// Identifies one displayable frame: a single frame, or a sum of `grouping` frames.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct LoadKey {
    pub start: usize,
    pub grouping: usize,
    /// Saturation the sum is clamped to. Only meaningful (non-zero) when grouping > 1.
    saturation: u16,
}

impl LoadKey {
    pub fn new(start: usize, grouping: usize, effective_saturation: u16) -> Self {
        let grouping = grouping.max(1);
        // Single frames don't depend on the saturation override, so don't let it
        // invalidate them.
        let saturation = if grouping > 1 { effective_saturation } else { 0 };
        Self { start, grouping, saturation }
    }
}

pub struct Loaded {
    pub key: LoadKey,
    pub result: Result<Arc<Frame>, String>,
}

#[derive(Clone, Copy)]
enum Job {
    /// The user wants to see this frame now.
    Display(LoadKey),
    /// Speculative read-ahead; dropped if anything more important is queued.
    Prefetch(LoadKey),
}

/// Reads HDF5 frames on a background thread so the UI never blocks on disk or
/// decompression, and keeps a few decoded frames for instant re-display.
///
/// The worker opens its own handle on the master file (the UI keeps its own
/// `Hdf5Series` for metadata). Dropping the loader stops the worker.
pub struct FrameLoader {
    tx: mpsc::Sender<Job>,
    rx: mpsc::Receiver<Loaded>,
    cache: VecDeque<(LoadKey, Arc<Frame>)>,
}

impl FrameLoader {
    pub fn spawn(master_path: PathBuf, ctx: egui::Context) -> Self {
        let (job_tx, job_rx) = mpsc::channel::<Job>();
        let (res_tx, res_rx) = mpsc::channel::<Loaded>();

        std::thread::Builder::new()
            .name("hdf5-loader".into())
            .spawn(move || worker(master_path, job_rx, res_tx, ctx))
            .expect("spawn hdf5-loader thread");

        Self { tx: job_tx, rx: res_rx, cache: VecDeque::new() }
    }

    pub fn get(&self, key: LoadKey) -> Option<Arc<Frame>> {
        self.cache.iter().find(|(k, _)| *k == key).map(|(_, f)| f.clone())
    }

    pub fn request(&self, key: LoadKey) {
        let _ = self.tx.send(Job::Display(key));
    }

    /// Ask for `key` in the background unless it is already cached.
    pub fn prefetch(&self, key: LoadKey) {
        if self.get(key).is_none() {
            let _ = self.tx.send(Job::Prefetch(key));
        }
    }

    /// Collect finished loads, caching the successful ones.
    pub fn poll(&mut self) -> Vec<Loaded> {
        let mut done = Vec::new();
        while let Ok(loaded) = self.rx.try_recv() {
            if let Ok(frame) = &loaded.result {
                self.cache.retain(|(k, _)| *k != loaded.key);
                self.cache.push_back((loaded.key, frame.clone()));
                while self.cache.len() > CACHE_CAPACITY {
                    self.cache.pop_front();
                }
            }
            done.push(loaded);
        }
        done
    }
}

fn worker(path: PathBuf, jobs: mpsc::Receiver<Job>, results: mpsc::Sender<Loaded>, ctx: egui::Context) {
    let series = match Hdf5Series::open(&path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("HDF5 loader: cannot open {}: {e:#}", path.display());
            return;
        }
    };

    while let Ok(first) = jobs.recv() {
        // Coalesce: only the newest display request matters; prefetches are
        // only worth doing if nothing was asked for explicitly.
        let mut pending = vec![first];
        pending.extend(jobs.try_iter());
        let job = pending
            .iter()
            .rev()
            .find(|j| matches!(j, Job::Display(_)))
            .or(pending.last())
            .expect("pending is never empty");
        let (Job::Display(key) | Job::Prefetch(key)) = *job;

        let result = load_group(&series, key).map(Arc::new).map_err(|e| format!("{e:#}"));
        if results.send(Loaded { key, result }).is_err() {
            return; // loader dropped
        }
        ctx.request_repaint();
    }
}

/// Load a single frame, or sum `key.grouping` consecutive frames (clamped to
/// `key.saturation`). Masked pixels count as zero in a sum.
fn load_group(series: &Hdf5Series, key: LoadKey) -> anyhow::Result<Frame> {
    if key.grouping <= 1 {
        return series.load_frame(key.start);
    }

    let end = (key.start + key.grouping).min(series.total_frames);
    let mut acc: Option<Vec<u32>> = None;
    let mut width = 0u32;
    let mut height = 0u32;
    let mut metadata = series.series_metadata.clone();
    let mut pixel_mask = None;

    for idx in key.start..end {
        match series.load_frame(idx) {
            Ok(frame) => {
                width = frame.width;
                height = frame.height;
                if idx == key.start {
                    metadata = frame.metadata.clone();
                    pixel_mask = frame.pixel_mask.clone();
                }
                match acc {
                    None => {
                        acc = Some(
                            frame
                                .pixels
                                .iter()
                                .enumerate()
                                .map(|(i, &v)| if frame.is_masked_index(i) { 0 } else { v as u32 })
                                .collect(),
                        )
                    }
                    Some(ref mut a) => {
                        for (i, (s, &v)) in a.iter_mut().zip(frame.pixels.iter()).enumerate() {
                            if !frame.is_masked_index(i) {
                                *s += v as u32;
                            }
                        }
                    }
                }
            }
            Err(e) => eprintln!("HDF5 grouped frame {idx}: {e:#}"),
        }
    }

    let acc = acc.ok_or_else(|| anyhow::anyhow!("no frames could be read for group starting at {}", key.start))?;
    let sat = key.saturation;
    let pixels: Vec<u16> = acc.iter().map(|&s| s.min(sat as u32) as u16).collect();
    Ok(Frame { pixels, pixel_mask, width, height, saturation_value: sat, metadata })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame() -> Arc<Frame> {
        Arc::new(Frame {
            pixels: vec![1],
            pixel_mask: None,
            width: 1,
            height: 1,
            saturation_value: 10,
            metadata: Default::default(),
        })
    }

    #[test]
    fn single_frame_keys_ignore_saturation_override() {
        assert_eq!(LoadKey::new(3, 1, 100), LoadKey::new(3, 1, 200));
        assert_eq!(LoadKey::new(3, 0, 100), LoadKey::new(3, 1, 200)); // grouping 0 acts as 1
        assert_ne!(LoadKey::new(3, 2, 100), LoadKey::new(3, 2, 200));
    }

    #[test]
    fn poll_caches_successes_evicts_oldest_and_reports_errors() {
        let (job_tx, _job_rx) = mpsc::channel();
        let (res_tx, res_rx) = mpsc::channel();
        let mut loader = FrameLoader { tx: job_tx, rx: res_rx, cache: VecDeque::new() };

        for i in 0..CACHE_CAPACITY + 2 {
            res_tx.send(Loaded { key: LoadKey::new(i, 1, 0), result: Ok(frame()) }).unwrap();
        }
        res_tx.send(Loaded { key: LoadKey::new(99, 1, 0), result: Err("boom".into()) }).unwrap();

        let done = loader.poll();
        assert_eq!(done.len(), CACHE_CAPACITY + 3);
        assert!(done.last().unwrap().result.is_err());
        assert!(loader.get(LoadKey::new(0, 1, 0)).is_none(), "oldest evicted");
        assert!(loader.get(LoadKey::new(CACHE_CAPACITY + 1, 1, 0)).is_some());
        assert!(loader.get(LoadKey::new(99, 1, 0)).is_none(), "errors are not cached");
    }
}
