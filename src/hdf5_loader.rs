use anyhow::{Context, Result, bail};
use hdf5_metno as hdf5;
use ndarray::s;
use std::path::Path;

use crate::frame::{Frame, FrameMetadata};

/// An open NXmx HDF5 series (one master file + N data files).
///
/// The master file is kept open for the lifetime of this struct so that
/// external links to the data files are resolved without re-opening.
pub struct Hdf5Series {
    master: hdf5::File,
    /// Total number of frames across all data files.
    pub total_frames: usize,
    /// Number of frames stored per data file.
    frames_per_file: usize,
    pub saturation_value: u16,
    /// Metadata that is the same for every frame in the series.
    pub series_metadata: FrameMetadata,
}

impl Hdf5Series {
    /// Open a master HDF5 file and read series-level metadata.
    ///
    /// `master_path` must be the `*_master.h5` file.  The data files
    /// (`*_data_000001.h5`, …) are expected to be in the same directory.
    pub fn open(master_path: &Path) -> Result<Self> {
        let master = hdf5::File::open(master_path)
            .with_context(|| format!("Cannot open master file: {}", master_path.display()))?;

        // --- Metadata ---
        let beam_center_x = read_scalar_f64(&master, "/entry/instrument/detector/beam_center_x").ok();
        let beam_center_y = read_scalar_f64(&master, "/entry/instrument/detector/beam_center_y").ok();
        let detector_distance = read_scalar_f64(&master, "/entry/instrument/detector/detector_distance").ok();
        let wavelength = read_scalar_f64(&master, "/entry/instrument/beam/incident_wavelength").ok();
        let pixel_size_x = read_scalar_f64(&master, "/entry/instrument/detector/x_pixel_size").ok();
        let pixel_size_y = read_scalar_f64(&master, "/entry/instrument/detector/y_pixel_size").ok();
        let exposure_time = read_scalar_f64(&master, "/entry/instrument/detector/count_time").ok();

        let series_metadata = FrameMetadata {
            beam_center_x,
            beam_center_y,
            detector_distance,
            wavelength,
            pixel_size_x,
            pixel_size_y,
            exposure_time,
            ..Default::default()
        };

        // --- Saturation value ---
        let saturation_value = master
            .dataset("/entry/instrument/detector/saturation_value")
            .ok()
            .and_then(|ds| read_scalar_i64(&ds))
            .map(|v| v.clamp(0, u16::MAX as i64) as u16)
            .unwrap_or(u16::MAX);
        eprintln!("HDF5: saturation_value = {saturation_value}");

        // --- Total frames: ntrigger * nimages ---
        let nimages = master
            .dataset("/entry/instrument/detector/detectorSpecific/nimages")
            .ok()
            .and_then(|ds| ds.read_scalar::<u64>().ok())
            .unwrap_or(0);
        let ntrigger = master
            .dataset("/entry/instrument/detector/detectorSpecific/ntrigger")
            .ok()
            .and_then(|ds| ds.read_scalar::<u64>().ok())
            .unwrap_or(1);
        let total_frames = (nimages * ntrigger) as usize;

        if total_frames == 0 {
            bail!("Cannot determine number of frames from master file");
        }

        // --- Count data file links under /entry/data ---
        let data_group = master.group("/entry/data").context("/entry/data group not found")?;
        let num_data_files = data_group.len() as usize;
        if num_data_files == 0 {
            bail!("No data files linked in /entry/data");
        }

        // Frames per file: all files have the same count except possibly the last.
        // Infer from shape of the first dataset.
        let frames_per_file = {
            let first_ds = master.dataset("/entry/data/data_000001").context("Cannot open /entry/data/data_000001")?;
            let shape = first_ds.shape();
            if shape.len() != 3 {
                bail!("Expected 3D dataset, got {} dims", shape.len());
            }
            shape[0]
        };

        Ok(Self {
            master,
            total_frames,
            frames_per_file,
            saturation_value,
            series_metadata,
        })
    }

    /// Load frame `index` (0-based) from the series.
    pub fn load_frame(&self, index: usize) -> Result<Frame> {
        if index >= self.total_frames {
            bail!("Frame index {index} out of range (total {})", self.total_frames);
        }

        let file_idx = index / self.frames_per_file + 1; // 1-based file number
        let within_idx = index % self.frames_per_file;

        // Path through the master file's external link.
        let ds_path = format!("/entry/data/data_{file_idx:06}");
        let ds = self.master.dataset(&ds_path).with_context(|| format!("Cannot open dataset {ds_path}"))?;

        let shape = ds.shape();
        if shape.len() != 3 {
            bail!("Expected 3D dataset in {ds_path}, got {} dims", shape.len());
        }
        let height = shape[1] as u32;
        let width = shape[2] as u32;

        let pixels = read_pixels_auto(&ds, within_idx)
            .with_context(|| format!("Cannot read frame {index} from {ds_path}"))?;

        let sat = self.saturation_value;
        let n_sat = pixels.iter().filter(|&&v| v >= sat).count();
        let max_px = pixels.iter().copied().max().unwrap_or(0);
        eprintln!("HDF5 frame {index}: sat_threshold={sat} pixels>={sat}: {n_sat} max_pixel={max_px}");

        let mut metadata = self.series_metadata.clone();
        metadata.image_number = Some(index as i64);

        Ok(Frame { pixels, width, height, saturation_value: sat, metadata })
    }
}

/// Read one 2-D frame from a 3-D dataset, trying common DECTRIS pixel dtypes.
///
/// EIGER 1 stores i16 (gap/masked pixels = −1, wraps to 65535 as u16).
/// EIGER 2 stores u32 (large dynamic range); some variants use u16 or i32.
/// HDF5 performs the conversion natively; we try each type and return the
/// first that succeeds.  On total failure we surface the i16 error because
/// it is the most likely to contain the real root cause (e.g., missing
/// bitshuffle filter, unexpected chunk layout).
fn read_pixels_auto(ds: &hdf5::Dataset, within_idx: usize) -> Result<Vec<u16>> {
    // Print dtype size before touching the read path so we always get this info.
    let dtype_size = ds.dtype().map(|dt| dt.size()).unwrap_or(0);

    // i16 — EIGER 1 (most common)
    match ds.read_slice_2d::<i16, _>(s![within_idx, .., ..]) {
        Ok(arr) => {
            return Ok(arr.as_slice().context("array not contiguous")?.iter().map(|&v| v as u16).collect());
        }
        Err(e_i16) => {
            // Print IMMEDIATELY — subsequent HDF5 calls may clear the error stack.
            eprintln!("  [hdf5] i16 read failed: {:?}", e_i16);

            // u32 — EIGER 2
            if let Ok(arr) = ds.read_slice_2d::<u32, _>(s![within_idx, .., ..]) {
                return Ok(arr.as_slice().context("array not contiguous")?.iter()
                    .map(|&v| v.min(u16::MAX as u32) as u16)
                    .collect());
            }
            // u16
            if let Ok(arr) = ds.read_slice_2d::<u16, _>(s![within_idx, .., ..]) {
                return Ok(arr.as_slice().context("array not contiguous")?.iter().copied().collect());
            }
            // i32
            if let Ok(arr) = ds.read_slice_2d::<i32, _>(s![within_idx, .., ..]) {
                return Ok(arr.as_slice().context("array not contiguous")?.iter().map(|&v| v as u16).collect());
            }

            Err(anyhow::anyhow!("dtype_size={dtype_size}B, read failed as i16/u32/u16/i32; see stderr for HDF5 error"))
        }
    }
}

/// Read a scalar (or 1-element array) dataset as i64, trying common integer types.
fn read_scalar_i64(ds: &hdf5::Dataset) -> Option<i64> {
    if let Ok(v) = ds.read_scalar::<i64>()  { return Some(v); }
    if let Ok(v) = ds.read_scalar::<i32>()  { return Some(v as i64); }
    if let Ok(v) = ds.read_scalar::<u32>()  { return Some(v as i64); }
    if let Ok(v) = ds.read_scalar::<i16>()  { return Some(v as i64); }
    if let Ok(v) = ds.read_scalar::<u16>()  { return Some(v as i64); }
    if let Ok(v) = ds.read_scalar::<u64>()  { return Some(v.min(i64::MAX as u64) as i64); }
    if let Ok(v) = ds.read_scalar::<f64>()  { return Some(v as i64); }
    if let Ok(v) = ds.read_scalar::<f32>()  { return Some(v as i64); }
    // Some files store it as a 1-element array.
    if let Ok(arr) = ds.read_1d::<i64>() { return arr.first().copied(); }
    if let Ok(arr) = ds.read_1d::<i32>() { return arr.first().map(|&v| v as i64); }
    if let Ok(arr) = ds.read_1d::<u32>() { return arr.first().map(|&v| v as i64); }
    if let Ok(arr) = ds.read_1d::<i16>() { return arr.first().map(|&v| v as i64); }
    if let Ok(arr) = ds.read_1d::<u16>() { return arr.first().map(|&v| v as i64); }
    eprintln!("HDF5: could not read saturation_value (dtype size={}B) — defaulting to u16::MAX",
        ds.dtype().map(|dt| dt.size()).unwrap_or(0));
    None
}

/// Read a scalar dataset as f64 (handles f32 and f64 source types).
fn read_scalar_f64(file: &hdf5::File, path: &str) -> Result<f64> {
    let ds = file.dataset(path)?;
    // Try f64 first, fall back to f32.
    if let Ok(v) = ds.read_scalar::<f64>() {
        return Ok(v);
    }
    let v = ds.read_scalar::<f32>().context("read_scalar f32")?;
    Ok(v as f64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_open_and_read_frame() {
        let master = std::path::Path::new("/home/ezepan/github/pumpkin/raw_data/lyzo-mono_9_master.h5");
        let series = Hdf5Series::open(master).expect("open master");

        assert_eq!(series.total_frames, 50000);
        assert_eq!(series.saturation_value, 32766);

        let meta = &series.series_metadata;
        assert!((meta.beam_center_x.unwrap() - 1717.6).abs() < 0.1);
        assert!((meta.beam_center_y.unwrap() - 1861.75).abs() < 0.1);
        assert!((meta.wavelength.unwrap() - 0.954872).abs() < 1e-4);

        let frame = series.load_frame(0).expect("load frame 0");
        assert_eq!(frame.width, 3106);
        assert_eq!(frame.height, 3264);
        assert_eq!(frame.pixels.len(), 3106 * 3264);
    }
}
