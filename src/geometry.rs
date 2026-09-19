use crate::frame::FrameMetadata;

/// Flat-detector diffraction geometry (detector perpendicular to the beam),
/// derived from frame metadata. Shared by the resolution rings, the PNG export
/// and the cursor resolution read-out so they can't disagree.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Geometry {
    /// Beam centre in image pixels.
    pub beam_center: (f64, f64),
    /// Wavelength in Å.
    pub wavelength: f64,
    /// Sample-to-detector distance in metres.
    pub distance: f64,
    /// Pixel size in metres (square pixels assumed).
    pub pixel_size: f64,
}

/// hc in eV·Å, used to derive wavelength from photon energy.
const HC_EV_ANGSTROM: f64 = 12398.42;

impl Geometry {
    /// Returns `None` if any required metadata is missing or non-physical.
    /// The wavelength falls back to the incident energy (eV) when absent or zero.
    pub fn from_metadata(meta: &FrameMetadata) -> Option<Self> {
        let wavelength = meta
            .wavelength
            .filter(|&w| w > 0.0)
            .or_else(|| meta.incident_energy.filter(|&e| e > 0.0).map(|e| HC_EV_ANGSTROM / e))?;
        let distance = meta.detector_distance.filter(|&d| d > 0.0)?;
        let pixel_size = meta.pixel_size_x.filter(|&p| p > 0.0)?;
        Some(Self {
            beam_center: (meta.beam_center_x?, meta.beam_center_y?),
            wavelength,
            distance,
            pixel_size,
        })
    }

    /// Radius in image pixels of the Debye-Scherrer ring for d-spacing `d` (Å).
    /// `None` if the ring doesn't exist (d < λ/2).
    pub fn ring_radius_px(&self, d: f64) -> Option<f64> {
        let sin_theta = self.wavelength / (2.0 * d);
        if !(sin_theta > 0.0 && sin_theta < 1.0) {
            return None;
        }
        let two_theta = 2.0 * sin_theta.asin();
        Some(self.distance * two_theta.tan() / self.pixel_size)
    }

    /// d-spacing (Å) at image pixel `(x, y)`; `None` at the beam centre.
    pub fn d_at_pixel(&self, x: f64, y: f64) -> Option<f64> {
        let dx = (x - self.beam_center.0) * self.pixel_size;
        let dy = (y - self.beam_center.1) * self.pixel_size;
        let r = dx.hypot(dy);
        if r == 0.0 {
            return None;
        }
        let sin_theta = ((r / self.distance).atan() / 2.0).sin();
        (sin_theta > 0.0).then(|| self.wavelength / (2.0 * sin_theta))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn geometry() -> Geometry {
        Geometry { beam_center: (100.0, 100.0), wavelength: 1.0, distance: 0.2, pixel_size: 75e-6 }
    }

    #[test]
    fn ring_radius_and_resolution_round_trip() {
        let g = geometry();
        for d in [1.5, 2.25, 3.67, 10.0] {
            let r = g.ring_radius_px(d).unwrap();
            let back = g.d_at_pixel(100.0 + r, 100.0).unwrap();
            assert!((back - d).abs() < 1e-9, "d={d} back={back}");
        }
    }

    #[test]
    fn ring_radius_matches_bragg_geometry() {
        // d = 2 Å, λ = 1 Å: 2θ = 2·asin(0.25); r = D·tan(2θ) / pixel.
        let expected = 0.2 * (2.0 * 0.25f64.asin()).tan() / 75e-6;
        assert!((geometry().ring_radius_px(2.0).unwrap() - expected).abs() < 1e-9);
    }

    #[test]
    fn unreachable_rings_and_beam_centre_give_none() {
        let g = geometry();
        assert!(g.ring_radius_px(0.4).is_none()); // d < λ/2
        assert!(g.d_at_pixel(100.0, 100.0).is_none());
    }

    #[test]
    fn wavelength_falls_back_to_energy() {
        let meta = FrameMetadata {
            beam_center_x: Some(1.0),
            beam_center_y: Some(1.0),
            detector_distance: Some(0.2),
            pixel_size_x: Some(75e-6),
            wavelength: Some(0.0),
            incident_energy: Some(12398.42),
            ..Default::default()
        };
        assert!((Geometry::from_metadata(&meta).unwrap().wavelength - 1.0).abs() < 1e-9);
    }
}
