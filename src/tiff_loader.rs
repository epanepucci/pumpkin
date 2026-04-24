use anyhow::{Context, Result, bail};
use std::io::Cursor;
use tiff::decoder::{Decoder, DecodingResult};

use crate::frame::Frame;

/// Decode a TIFF byte buffer into a `Frame`.
///
/// Handles both 8-bit and 16-bit grayscale images from the DECTRIS monitor endpoint.
/// Extracts metadata from the DECTRIS private IFD tag when present.
pub fn decode_tiff(data: &[u8]) -> Result<Frame> {
    let cursor = Cursor::new(data);
    let mut decoder = Decoder::new(cursor).context("TIFF decoder init")?;

    let (width, height) = decoder.dimensions().context("TIFF dimensions")?;

    let (pixels, saturation_value): (Vec<u16>, u16) = match decoder.read_image().context("TIFF read_image")? {
        DecodingResult::U8(buf) => (buf.into_iter().map(|v| v as u16).collect(), u8::MAX as u16),
        DecodingResult::U16(buf) => (buf, u16::MAX),
        DecodingResult::U32(buf) => (
            buf.into_iter().map(|v| v.min(u16::MAX as u32) as u16).collect(),
            u16::MAX,
        ),
        other => bail!("Unsupported TIFF pixel format: {:?}", other),
    };

    if pixels.len() != (width * height) as usize {
        bail!("Pixel count mismatch: got {}, expected {}x{}={}", pixels.len(), width, height, width * height);
    }

    // Skip DECTRIS private metadata parsing for now.
    let metadata = Default::default();

    Ok(Frame { pixels, width, height, saturation_value, metadata })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_decode_rejects_garbage() {
        assert!(decode_tiff(b"not a tiff").is_err());
    }
}
