use anyhow::{Context, Result};
use std::io::{Cursor, Read, Seek, SeekFrom};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;

use crate::frame::{Frame, FrameMetadata};
use crate::tiff_loader::decode_tiff;

/// Configuration for the monitor polling task.
#[derive(Clone, Debug)]
pub struct MonitorConfig {
    /// Base URL of the DCU, e.g. "http://192.168.1.1".
    pub dcu_url: String,
    /// SIMPLON API version string, e.g. "1.8.0".
    pub api_version: String,
}

impl MonitorConfig {
    /// `GET /images/` — returns the buffer list.
    pub fn images_list_url(&self) -> String {
        format!("{}/monitor/api/{}/images/", self.dcu_url, self.api_version)
    }

    /// `GET /images/<series>/<id>/0` — fetch a specific frame (threshold 0).
    pub fn image_url(&self, series: u64, image_id: u64) -> String {
        format!("{}/monitor/api/{}/images/{}/{}", self.dcu_url, self.api_version, series, image_id)
    }

    pub fn config_url(&self, param: &str) -> String {
        format!("{}/monitor/api/{}/config/{}", self.dcu_url, self.api_version, param)
    }
}

/// Frames delivered to the app for the current series.
///
/// When the last series has ≤4 images all are pre-fetched (browse mode).
/// When >4 images only the latest frame is included (live mode).
#[derive(Clone)]
pub struct MonitorBatch {
    pub series_id: u64,
    /// All image IDs present in the buffer for this series.
    pub image_ids: Vec<u64>,
    pub frames: Vec<Arc<Frame>>,
}

#[allow(dead_code)]
/// Enable the monitor interface on the detector (blocking helper).
pub fn enable_monitor(client: &reqwest::blocking::Client, cfg: &MonitorConfig) -> Result<()> {
    let url = cfg.config_url("mode");
    let body = serde_json::json!({ "value": "enabled" });
    let resp = client.put(&url).json(&body).send().context("PUT monitor mode")?;
    if !resp.status().is_success() {
        anyhow::bail!("Enable monitor failed: HTTP {}", resp.status());
    }
    Ok(())
}

/// Spawn a background task that polls the monitor buffer list and sends decoded
/// frames over a `watch` channel. Returns the receiver.
pub fn start_monitor_task(cfg: MonitorConfig) -> watch::Receiver<Option<MonitorBatch>> {
    let (tx, rx) = watch::channel(None);

    tokio::spawn(async move {
        let client = match reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(120))
            .build()
        {
            Ok(c) => c,
            Err(e) => {
                eprintln!("Monitor: failed to build HTTP client: {e}");
                return;
            }
        };

        // Enable monitor mode.
        let enable_url = cfg.config_url("mode");
        let enable_body = serde_json::json!({ "value": "enabled" });
        eprintln!("Monitor: enabling monitor mode via PUT {enable_url}");
        match client.put(&enable_url).json(&enable_body).send().await {
            Err(e) => eprintln!("Monitor: could not enable monitor mode: {e}"),
            Ok(r) => eprintln!("Monitor: enable response HTTP {}", r.status()),
        }

        let mut known_series_id: Option<u64> = None;
        let mut known_image_count: usize = 0;

        loop {
            if tx.is_closed() {
                break;
            }

            let list_url = cfg.images_list_url();
            let buffer_list = match fetch_buffer_list(&client, &list_url).await {
                Ok(list) => list,
                Err(e) => {
                    eprintln!("Monitor: buffer list error: {e}");
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    continue;
                }
            };

            let Some((series_id, image_ids)) = buffer_list.last().cloned() else {
                tokio::time::sleep(Duration::from_millis(500)).await;
                continue;
            };

            let image_count = image_ids.len();
            let changed = Some(series_id) != known_series_id || image_count != known_image_count;
            if !changed {
                tokio::time::sleep(Duration::from_millis(500)).await;
                continue;
            }

            eprintln!("Monitor: buffer changed — series {series_id}, {image_count} images");

            // Fetch all frames when ≤4 (browse mode), only the latest when >4.
            let ids_to_fetch: Vec<u64> =
                if image_count <= 4 { image_ids.clone() } else { vec![*image_ids.last().unwrap()] };

            let mut frames = Vec::with_capacity(ids_to_fetch.len());
            for &image_id in &ids_to_fetch {
                let url = cfg.image_url(series_id, image_id);
                eprintln!("Monitor: fetching {url}");
                eprintln!("Monitor: fetching {series_id}/{image_id}");
                match fetch_tiff(&client, &url).await {
                    Ok(frame) => frames.push(Arc::new(frame)),
                    Err(e) => eprintln!("Monitor: fetch {series_id}/{image_id} failed: {e}"),
                }
            }

            if !frames.is_empty() {
                known_series_id = Some(series_id);
                known_image_count = image_count;
                let _ = tx.send(Some(MonitorBatch { series_id, image_ids, frames }));
            }

            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    });

    rx
}

/// Parse `GET /images/` response: `[[series_id, [img_id, ...]], ...]`
async fn fetch_buffer_list(client: &reqwest::Client, url: &str) -> Result<Vec<(u64, Vec<u64>)>> {
    let resp = client.get(url).send().await.context("GET images list")?;
    if !resp.status().is_success() {
        anyhow::bail!("images list HTTP {}", resp.status());
    }
    let json: serde_json::Value = resp.json().await.context("parse images list")?;
    let arr = json.as_array().context("expected JSON array")?;
    let mut result = Vec::with_capacity(arr.len());
    for entry in arr {
        let pair = entry.as_array().context("expected [series, [ids]]")?;
        anyhow::ensure!(pair.len() >= 2, "buffer list entry too short");
        let series_id = pair[0].as_u64().context("series_id not u64")?;
        let ids: Vec<u64> =
            pair[1].as_array().context("image ids not array")?.iter().filter_map(|v| v.as_u64()).collect();
        result.push((series_id, ids));
    }
    Ok(result)
}

async fn fetch_tiff(client: &reqwest::Client, url: &str) -> Result<Frame> {
    let resp = client.get(url).send().await.context("GET image")?;
    if !resp.status().is_success() {
        anyhow::bail!("image HTTP {}", resp.status());
    }
    let bytes = resp.bytes().await.context("read body")?;
    decode_tiff(&bytes)
}

/// Parse the DECTRIS private IFD tag (0xC7F8) from the raw TIFF bytes.
#[allow(dead_code)]
fn parse_dectris_metadata(data: &[u8]) -> Result<FrameMetadata> {
    const DECTRIS_TAG: u32 = 0xC7F8;
    const TAG_SERIES_NUMBER: u16 = 0x0002;
    const TAG_IMAGE_NUMBER: u16 = 0x0003;
    const TAG_EXPOSURE_TIME: u16 = 0x0007;
    const TAG_BEAM_CENTER: u16 = 0x0016;
    const TAG_DETECTOR_DISTANCE: u16 = 0x0017;

    let mut cur = Cursor::new(data);

    let mut bom = [0u8; 2];
    cur.read_exact(&mut bom)?;
    let little_endian = match &bom {
        b"II" => true,
        b"MM" => false,
        _ => anyhow::bail!("Not a TIFF file"),
    };

    let read_u16 = |cur: &mut Cursor<&[u8]>| -> Result<u16> {
        let mut buf = [0u8; 2];
        cur.read_exact(&mut buf)?;
        Ok(if little_endian { u16::from_le_bytes(buf) } else { u16::from_be_bytes(buf) })
    };

    let read_u32 = |cur: &mut Cursor<&[u8]>| -> Result<u32> {
        let mut buf = [0u8; 4];
        cur.read_exact(&mut buf)?;
        Ok(if little_endian { u32::from_le_bytes(buf) } else { u32::from_be_bytes(buf) })
    };

    let read_f64_at = |cur: &mut Cursor<&[u8]>, offset: u64| -> Result<f64> {
        cur.seek(SeekFrom::Start(offset))?;
        let mut buf = [0u8; 8];
        cur.read_exact(&mut buf)?;
        Ok(if little_endian { f64::from_le_bytes(buf) } else { f64::from_be_bytes(buf) })
    };

    let magic = read_u16(&mut cur)?;
    if magic != 42 {
        anyhow::bail!("TIFF magic mismatch: {}", magic);
    }

    let ifd_offset = read_u32(&mut cur)? as u64;
    cur.seek(SeekFrom::Start(ifd_offset))?;
    let entry_count = read_u16(&mut cur)?;

    let mut dectris_offset: Option<u64> = None;
    for _ in 0..entry_count {
        let tag = read_u16(&mut cur)?;
        let _type_ = read_u16(&mut cur)?;
        let _count = read_u32(&mut cur)?;
        let value_or_offset = read_u32(&mut cur)?;
        if tag == DECTRIS_TAG as u16 {
            dectris_offset = Some(value_or_offset as u64);
        }
    }

    let sub_ifd_offset = match dectris_offset {
        Some(o) => o,
        None => return Ok(FrameMetadata::default()),
    };

    cur.seek(SeekFrom::Start(sub_ifd_offset))?;
    let sub_entry_count = read_u16(&mut cur)?;
    let mut meta = FrameMetadata::default();

    for _ in 0..sub_entry_count {
        let tag = read_u16(&mut cur)?;
        let type_ = read_u16(&mut cur)?;
        let count = read_u32(&mut cur)?;
        let val_pos = cur.stream_position()?;
        let value_or_offset = read_u32(&mut cur)?;

        match tag {
            TAG_EXPOSURE_TIME => {
                if type_ == 12 && count == 1 {
                    if let Ok(v) = read_f64_at(&mut cur, value_or_offset as u64) {
                        meta.exposure_time = Some(v);
                    }
                }
            }
            TAG_BEAM_CENTER => {
                if type_ == 12 && count == 2 {
                    let offset = value_or_offset as u64;
                    if let (Ok(x), Ok(y)) = (read_f64_at(&mut cur, offset), read_f64_at(&mut cur, offset + 8)) {
                        meta.beam_center_x = Some(x);
                        meta.beam_center_y = Some(y);
                    }
                }
            }
            TAG_DETECTOR_DISTANCE => {
                if type_ == 12 && count == 1 {
                    if let Ok(v) = read_f64_at(&mut cur, value_or_offset as u64) {
                        meta.detector_distance = Some(v);
                    }
                }
            }
            TAG_IMAGE_NUMBER => {
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
