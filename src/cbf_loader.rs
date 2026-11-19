use anyhow::{Context, Result, bail};
use std::path::Path;
use std::sync::Arc;

use crate::frame::{Frame, FrameMetadata};

/// Marks the start of the binary section, right after the MIME header block.
const BINARY_START: [u8; 4] = [0x0C, 0x1A, 0x04, 0xD5];

/// True if `path` has a `.cbf` extension (case-insensitive).
pub fn is_cbf_path(path: &Path) -> bool {
    path.extension().and_then(|e| e.to_str()).is_some_and(|e| e.eq_ignore_ascii_case("cbf"))
}

/// Read a single-image CBF file (e.g. from a PILATUS or EIGER detector).
pub fn load_cbf(path: &Path) -> Result<Frame> {
    let data = std::fs::read(path).with_context(|| format!("Cannot read {}", path.display()))?;
    decode_cbf(&data).with_context(|| format!("Cannot decode CBF {}", path.display()))
}

/// Decode a CBF byte buffer into a `Frame`.
///
/// Only the `x-CBF_BYTE_OFFSET` compression used by DECTRIS detectors is supported.
/// Negative pixel values (gaps = -1, bad pixels = -2) become masked pixels; values
/// above `u16::MAX` are clamped, as for 32-bit HDF5 data. Experiment metadata is
/// read from the SLS/DECTRIS "mini-CBF" header lines when present.
pub fn decode_cbf(data: &[u8]) -> Result<Frame> {
    let start = data
        .windows(BINARY_START.len())
        .position(|w| w == BINARY_START)
        .context("binary section start marker not found")?;
    let header = String::from_utf8_lossy(&data[..start]);
    let binary = &data[start + BINARY_START.len()..];

    let conversions = mime_field(&header, "conversions").unwrap_or_default();
    if !conversions.eq_ignore_ascii_case("x-CBF_BYTE_OFFSET") {
        bail!("unsupported CBF compression {conversions:?} (only x-CBF_BYTE_OFFSET is supported)");
    }
    let width: u32 = mime_number(&header, "X-Binary-Size-Fastest-Dimension")?;
    let height: u32 = mime_number(&header, "X-Binary-Size-Second-Dimension")?;
    let n = width as usize * height as usize;
    if let Ok(declared) = mime_number::<usize>(&header, "X-Binary-Number-of-Elements") {
        if declared != n {
            bail!("element count mismatch: header says {declared}, expected {width}x{height}={n}");
        }
    }
    let binary = match mime_number::<usize>(&header, "X-Binary-Size") {
        Ok(size) if size <= binary.len() => &binary[..size],
        _ => binary,
    };

    let values = decompress_byte_offset(binary, n)?;

    let mut pixels = Vec::with_capacity(n);
    let mut mask = vec![0u8; n];
    let mut any_masked = false;
    for (i, &v) in values.iter().enumerate() {
        if v < 0 {
            mask[i] = 1;
            any_masked = true;
            pixels.push(0);
        } else {
            pixels.push(v.min(u16::MAX as i64) as u16);
        }
    }

    let metadata = parse_mini_cbf_header(&header);
    let saturation_value = header_line(&header, "Count_cutoff")
        .and_then(|rest| first_number(rest))
        .map(|c| c.clamp(1.0, u16::MAX as f64) as u16)
        .unwrap_or(u16::MAX);

    Ok(Frame {
        pixels,
        pixel_mask: any_masked.then(|| Arc::from(mask)),
        width,
        height,
        saturation_value,
        metadata,
    })
}

/// Undo CBF byte-offset compression: each value is a delta from the previous one,
/// stored as i8, escaping to i16, i32 and then i64 when the delta doesn't fit.
fn decompress_byte_offset(data: &[u8], n: usize) -> Result<Vec<i64>> {
    let mut out = Vec::with_capacity(n);
    let mut pos = 0;
    let mut value: i64 = 0;
    let truncated = || anyhow::anyhow!("compressed data truncated");
    while out.len() < n {
        let d8 = *data.get(pos).ok_or_else(truncated)? as i8;
        pos += 1;
        let delta = if d8 != i8::MIN {
            d8 as i64
        } else {
            let b = data.get(pos..pos + 2).ok_or_else(truncated)?;
            pos += 2;
            let d16 = i16::from_le_bytes([b[0], b[1]]);
            if d16 != i16::MIN {
                d16 as i64
            } else {
                let b = data.get(pos..pos + 4).ok_or_else(truncated)?;
                pos += 4;
                let d32 = i32::from_le_bytes([b[0], b[1], b[2], b[3]]);
                if d32 != i32::MIN {
                    d32 as i64
                } else {
                    let b = data.get(pos..pos + 8).ok_or_else(truncated)?;
                    pos += 8;
                    i64::from_le_bytes(b.try_into().unwrap())
                }
            }
        };
        value = value.wrapping_add(delta);
        out.push(value);
    }
    Ok(out)
}

/// Value of a `Name: value` (or `name="value"`) MIME header field, unquoted.
fn mime_field<'a>(header: &'a str, name: &str) -> Option<&'a str> {
    header.lines().find_map(|line| {
        let line = line.trim();
        let rest = line.get(..name.len()).filter(|p| p.eq_ignore_ascii_case(name)).map(|_| &line[name.len()..])?;
        let rest = rest.trim_start().strip_prefix([':', '='])?;
        Some(rest.trim().trim_end_matches(';').trim_matches('"'))
    })
}

fn mime_number<T: std::str::FromStr>(header: &str, name: &str) -> Result<T> {
    mime_field(header, name)
        .with_context(|| format!("missing {name} in CBF header"))?
        .parse()
        .map_err(|_| anyhow::anyhow!("invalid {name} in CBF header"))
}

/// Text after `# <key>` in a mini-CBF header line, e.g. `Wavelength 1.0 A` -> `1.0 A`.
fn header_line<'a>(header: &'a str, key: &str) -> Option<&'a str> {
    header.lines().find_map(|line| {
        let rest = line.trim().strip_prefix('#')?.trim_start().strip_prefix(key)?;
        // Require a separator so e.g. "Exposure_time" doesn't match "Exposure_time_x".
        rest.starts_with([' ', ':', '=', '\t']).then(|| rest.trim_start_matches([' ', ':', '=', '\t']))
    })
}

/// All numbers in `s`, ignoring units and punctuation like `(244, 308)` or `172e-6 m x 172e-6 m`.
fn numbers(s: &str) -> Vec<f64> {
    s.split(|c: char| c.is_whitespace() || matches!(c, '(' | ')' | ','))
        .filter_map(|tok| tok.parse().ok())
        .collect()
}

fn first_number(s: &str) -> Option<f64> {
    numbers(s).first().copied()
}

fn parse_mini_cbf_header(header: &str) -> FrameMetadata {
    let mut m = FrameMetadata::default();
    if let Some(v) = header_line(header, "Pixel_size").map(numbers) {
        m.pixel_size_x = v.first().copied();
        m.pixel_size_y = v.get(1).copied().or(m.pixel_size_x);
    }
    if let Some(v) = header_line(header, "Beam_xy").map(numbers) {
        m.beam_center_x = v.first().copied();
        m.beam_center_y = v.get(1).copied();
    }
    m.exposure_time = header_line(header, "Exposure_time").and_then(first_number);
    m.wavelength = header_line(header, "Wavelength").and_then(first_number);
    m.detector_distance = header_line(header, "Detector_distance").and_then(first_number);
    m.incident_energy = m.wavelength.filter(|&w| w > 0.0).map(|w| 12398.42 / w);
    m.nimages = Some(1);
    m
}

#[cfg(test)]
mod tests {
    use super::*;

    fn compress_byte_offset(values: &[i64]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut prev = 0i64;
        for &v in values {
            let d = v - prev;
            prev = v;
            if (-127..=127).contains(&d) {
                out.push(d as i8 as u8);
            } else if (-32767..=32767).contains(&d) {
                out.push(0x80);
                out.extend((d as i16).to_le_bytes());
            } else {
                out.push(0x80);
                out.extend(i16::MIN.to_le_bytes());
                out.extend((d as i32).to_le_bytes());
            }
        }
        out
    }

    fn make_cbf(width: u32, height: u32, values: &[i64]) -> Vec<u8> {
        let binary = compress_byte_offset(values);
        let mut out = format!(
            "###CBF: VERSION 1.5\r\n\
             _array_data.header_contents\r\n\
             ;\r\n\
             # Pixel_size 75e-6 m x 75e-6 m\r\n\
             # Exposure_time 0.1000000 s\r\n\
             # Count_cutoff 1000 counts\r\n\
             # Beam_xy (1.50, 2.25) pixels\r\n\
             # Wavelength 1.0000 A\r\n\
             # Detector_distance 0.25000 m\r\n\
             ;\r\n\
             _array_data.data\r\n\
             ;\r\n\
             --CIF-BINARY-FORMAT-SECTION--\r\n\
             Content-Type: application/octet-stream;\r\n     conversions=\"x-CBF_BYTE_OFFSET\"\r\n\
             X-Binary-Size: {}\r\n\
             X-Binary-Element-Type: \"signed 32-bit integer\"\r\n\
             X-Binary-Number-of-Elements: {}\r\n\
             X-Binary-Size-Fastest-Dimension: {width}\r\n\
             X-Binary-Size-Second-Dimension: {height}\r\n\r\n",
            binary.len(),
            values.len(),
        )
        .into_bytes();
        out.extend(BINARY_START);
        out.extend(binary);
        out.extend(TRAILER);
        out
    }

    const TRAILER: &[u8] = b"\r\n--CIF-BINARY-FORMAT-SECTION----\r\n;\r\n";

    #[test]
    fn decodes_pixels_mask_and_metadata() {
        let values = [0, 5, -1, 300, 100_000, 7, -2, 40_000_000];
        let frame = decode_cbf(&make_cbf(4, 2, &values)).unwrap();
        assert_eq!((frame.width, frame.height), (4, 2));
        assert_eq!(frame.pixels, vec![0, 5, 0, 300, u16::MAX, 7, 0, u16::MAX]);
        assert!(frame.is_masked_index(2));
        assert!(frame.is_masked_index(6));
        assert!(!frame.is_masked_index(3));
        assert_eq!(frame.saturation_value, 1000);
        let m = &frame.metadata;
        assert_eq!(m.pixel_size_x, Some(75e-6));
        assert_eq!(m.pixel_size_y, Some(75e-6));
        assert_eq!((m.beam_center_x, m.beam_center_y), (Some(1.5), Some(2.25)));
        assert_eq!(m.wavelength, Some(1.0));
        assert_eq!(m.detector_distance, Some(0.25));
        assert_eq!(m.exposure_time, Some(0.1));
    }

    #[test]
    fn rejects_truncated_and_garbage() {
        let mut cbf = make_cbf(4, 2, &[1, 2, 3, 4, 5, 6, 7, 300]);
        cbf.truncate(cbf.len() - TRAILER.len() - 2);
        assert!(decode_cbf(&cbf).is_err());
        assert!(decode_cbf(b"not a cbf").is_err());
    }
}
