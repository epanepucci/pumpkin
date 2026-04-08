use anyhow::{bail, Context, Result};
use std::io::{Cursor, Read, Seek, SeekFrom};
use tiff::decoder::{Decoder, DecodingResult};
use tiff::ColorType;

use crate::frame::{Frame, FrameMetadata};

/// DECTRIS private TIFF tag number (decimal 51192, hex 0xC7F8).
const DECTRIS_TAG: u32 = 0xC7F8;

// DECTRIS sub-IFD tag codes (Table 5.10 in SIMPLON docs).
#[allow(dead_code)]
const TAG_SERIES_UNIQUE_ID: u16 = 0x0001;
const TAG_SERIES_NUMBER: u16 = 0x0002;
const TAG_IMAGE_NUMBER: u16 = 0x0003;
const TAG_EXPOSURE_TIME: u16 = 0x0007;
const TAG_BEAM_CENTER: u16 = 0x0016;
const TAG_DETECTOR_DISTANCE: u16 = 0x0017;

/// Decode a TIFF byte buffer into a `Frame`.
///
/// Handles both 8-bit and 16-bit grayscale images from the DECTRIS monitor endpoint.
/// Extracts metadata from the DECTRIS private IFD tag when present.
pub fn decode_tiff(data: &[u8]) -> Result<Frame> {
    let cursor = Cursor::new(data);
    let mut decoder = Decoder::new(cursor).context("TIFF decoder init")?;

    let (width, height) = decoder.dimensions().context("TIFF dimensions")?;
    let color_type = decoder.colortype().context("TIFF color type")?;

    let pixels: Vec<u16> = match decoder.read_image().context("TIFF read_image")? {
        DecodingResult::U8(buf) => buf.into_iter().map(|v| v as u16).collect(),
        DecodingResult::U16(buf) => buf,
        other => bail!("Unsupported TIFF pixel format: {:?}", other),
    };

    if pixels.len() != (width * height) as usize {
        bail!(
            "Pixel count mismatch: got {}, expected {}x{}={}",
            pixels.len(),
            width,
            height,
            width * height
        );
    }

    // Determine saturation value from bit depth.
    let saturation_value = match color_type {
        ColorType::Gray(8) => u8::MAX as u16,
        ColorType::Gray(16) => u16::MAX,
        _ => u16::MAX,
    };

    // Parse DECTRIS private metadata from the raw bytes.
    let metadata = parse_dectris_metadata(data).unwrap_or_default();

    Ok(Frame {
        pixels,
        width,
        height,
        saturation_value,
        metadata,
    })
}

/// Parse the DECTRIS private IFD tag (0xC7F8) from the raw TIFF bytes.
///
/// The `tiff` crate doesn't expose unknown private tags, so we parse the raw bytes
/// manually. We locate the IFD, find tag 0xC7F8, then parse its sub-IFD.
fn parse_dectris_metadata(data: &[u8]) -> Result<FrameMetadata> {
    let mut cur = Cursor::new(data);

    // Read TIFF byte order marker.
    let mut bom = [0u8; 2];
    cur.read_exact(&mut bom)?;
    let little_endian = match &bom {
        b"II" => true,
        b"MM" => false,
        _ => bail!("Not a TIFF file"),
    };

    let read_u16 = |cur: &mut Cursor<&[u8]>| -> Result<u16> {
        let mut buf = [0u8; 2];
        cur.read_exact(&mut buf)?;
        Ok(if little_endian {
            u16::from_le_bytes(buf)
        } else {
            u16::from_be_bytes(buf)
        })
    };

    let read_u32 = |cur: &mut Cursor<&[u8]>| -> Result<u32> {
        let mut buf = [0u8; 4];
        cur.read_exact(&mut buf)?;
        Ok(if little_endian {
            u32::from_le_bytes(buf)
        } else {
            u32::from_be_bytes(buf)
        })
    };

    let read_f64_at = |cur: &mut Cursor<&[u8]>, offset: u64| -> Result<f64> {
        cur.seek(SeekFrom::Start(offset))?;
        let mut buf = [0u8; 8];
        cur.read_exact(&mut buf)?;
        Ok(if little_endian {
            f64::from_le_bytes(buf)
        } else {
            f64::from_be_bytes(buf)
        })
    };

    // Verify magic number (42).
    let magic = read_u16(&mut cur)?;
    if magic != 42 {
        bail!("TIFF magic mismatch: {}", magic);
    }

    // Read offset to first IFD.
    let ifd_offset = read_u32(&mut cur)? as u64;
    cur.seek(SeekFrom::Start(ifd_offset))?;

    let entry_count = read_u16(&mut cur)?;

    // Scan IFD entries for the DECTRIS private tag.
    let mut dectris_offset: Option<u64> = None;

    for _ in 0..entry_count {
        let tag = read_u16(&mut cur)?;
        let _type_ = read_u16(&mut cur)?;
        let _count = read_u32(&mut cur)?;
        let value_or_offset_pos = cur.stream_position()?;
        let value_or_offset = read_u32(&mut cur)?;

        if tag == DECTRIS_TAG as u16 {
            // The value field holds the offset to the DECTRIS sub-IFD.
            dectris_offset = Some(value_or_offset as u64);
        }
        let _ = value_or_offset_pos; // suppress unused warning
    }

    let sub_ifd_offset = match dectris_offset {
        Some(o) => o,
        None => return Ok(FrameMetadata::default()),
    };

    // Parse the DECTRIS sub-IFD.
    cur.seek(SeekFrom::Start(sub_ifd_offset))?;
    let sub_entry_count = read_u16(&mut cur)?;

    let mut meta = FrameMetadata::default();

    for _ in 0..sub_entry_count {
        let tag = read_u16(&mut cur)?;
        let type_ = read_u16(&mut cur)?; // 12 = DOUBLE, 4 = LONG
        let count = read_u32(&mut cur)?;
        let val_pos = cur.stream_position()?;
        let value_or_offset = read_u32(&mut cur)?;

        match tag {
            TAG_EXPOSURE_TIME => {
                // DOUBLE (type 12), count=1, value is at offset.
                if type_ == 12 && count == 1 {
                    if let Ok(v) = read_f64_at(&mut cur, value_or_offset as u64) {
                        meta.exposure_time = Some(v);
                    }
                }
            }
            TAG_BEAM_CENTER => {
                // DOUBLE (type 12), count=2: [x, y]
                if type_ == 12 && count == 2 {
                    let offset = value_or_offset as u64;
                    if let (Ok(x), Ok(y)) = (
                        read_f64_at(&mut cur, offset),
                        read_f64_at(&mut cur, offset + 8),
                    ) {
                        meta.beam_center_x = Some(x);
                        meta.beam_center_y = Some(y);
                    }
                }
            }
            TAG_DETECTOR_DISTANCE => {
                // DOUBLE (type 12), count=1.
                if type_ == 12 && count == 1 {
                    if let Ok(v) = read_f64_at(&mut cur, value_or_offset as u64) {
                        meta.detector_distance = Some(v);
                    }
                }
            }
            TAG_IMAGE_NUMBER => {
                // LONG (type 4), count=1. Value fits in the 4-byte field.
                if type_ == 4 && count == 1 {
                    meta.image_number = Some(value_or_offset as i64);
                }
            }
            TAG_SERIES_NUMBER => {
                if type_ == 4 && count == 1 {
                    meta.series_id = Some(value_or_offset as i64);
                }
            }
            _ => {}
        }

        // Restore position after the 4-byte value/offset field.
        cur.seek(SeekFrom::Start(val_pos + 4))?;
    }

    Ok(meta)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_decode_rejects_garbage() {
        assert!(decode_tiff(b"not a tiff").is_err());
    }
}
