/// A decoded image frame with pixel data and experiment metadata.
#[derive(Clone)]
pub struct Frame {
    /// Raw pixel values, row-major, u16 per pixel.
    pub pixels: Vec<u16>,
    pub width: u32,
    pub height: u32,

    /// Pixels at or above this value are saturated/overloaded.
    pub saturation_value: u16,

    pub metadata: FrameMetadata,
}

/// Experiment metadata extracted from the DECTRIS TIFF private tag or stream headers.
#[derive(Clone, Debug, Default)]
pub struct FrameMetadata {
    /// Beam center in detector pixel coordinates.
    pub beam_center_x: Option<f64>,
    pub beam_center_y: Option<f64>,

    /// Sample-to-detector distance in metres.
    pub detector_distance: Option<f64>,

    /// Incident photon wavelength in Angstroms.
    pub wavelength: Option<f64>,

    /// Pixel size in metres (x and y).
    pub pixel_size_x: Option<f64>,
    pub pixel_size_y: Option<f64>,

    /// Incident beam energy in eV.
    pub incident_energy: Option<f64>,

    /// Exposure time in seconds - count_time from SIMPLON
    pub exposure_time: Option<f64>,

    /// Total number of images in the series.
    pub nimages: Option<u64>,

    /// Total number of triggers in the series.
    pub ntrigger: Option<u64>,

    /// FileWriter name pattern for the series.
    pub name_pattern: Option<String>,

    pub series_id: Option<i64>,

    pub image_number: Option<i64>,
    /// Date and time when data collection started, as an ISO 8601 string.
    pub data_collection_date: Option<String>,
}

impl Frame {
    /// Returns true if the given pixel value is at saturation.
    pub fn is_saturated(&self, value: u16) -> bool {
        value >= self.saturation_value
    }
}
