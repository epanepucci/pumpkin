use anyhow::{Context, Result};
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
    /// How often to poll the buffer list for new images (milliseconds).
    pub poll_period_ms: u64,
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

    pub fn detector_config_url(&self, param: &str) -> String {
        format!("{}/detector/api/{}/config/{}", self.dcu_url, self.api_version, param)
    }

    pub fn filewriter_config_url(&self, param: &str) -> String {
        format!("{}/filewriter/api/{}/config/{}", self.dcu_url, self.api_version, param)
    }
}

/// Frames delivered to the app for the current series.
///
/// When the last series has ≤4 images all are pre-fetched (browse mode).
/// When >4 images only the latest frame is included (live mode).
#[derive(Clone)]
pub struct MonitorBatch {
    pub series_id: u64,
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
        let mut known_meta = FrameMetadata::default();

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
                tokio::time::sleep(Duration::from_millis(cfg.poll_period_ms)).await;
                continue;
            };

            let image_count = image_ids.len();
            let new_series = Some(series_id) != known_series_id;
            let changed = new_series || image_count != known_image_count;
            if !changed {
                tokio::time::sleep(Duration::from_millis(cfg.poll_period_ms)).await;
                continue;
            }

            eprintln!("Monitor: buffer changed — series {series_id}, {image_count} images");

            // Fetch detector/filewriter config on each new series.
            if new_series {
                eprintln!("Monitor: fetching series metadata for series {series_id}");
                known_meta = fetch_series_metadata(&client, &cfg).await;
                known_meta.series_id = Some(series_id as i64);
            }

            // Fetch all frames when ≤4 (browse mode), only the latest when >4.
            let ids_to_fetch: Vec<u64> =
                if image_count <= 4 { image_ids.clone() } else { vec![*image_ids.last().unwrap()] };

            let mut frames = Vec::with_capacity(ids_to_fetch.len());
            for &image_id in &ids_to_fetch {
                let url = cfg.image_url(series_id, image_id);
                eprintln!("Monitor: fetching {series_id}/{image_id}");
                match fetch_tiff(&client, &url).await {
                    Ok(mut frame) => {
                        frame.metadata = known_meta.clone();
                        frame.metadata.image_number = Some(image_id as i64);
                        frames.push(Arc::new(frame));
                    }
                    Err(e) => eprintln!("Monitor: fetch {series_id}/{image_id} failed: {e}"),
                }
            }

            if !frames.is_empty() {
                known_series_id = Some(series_id);
                known_image_count = image_count;
                let _ = tx.send(Some(MonitorBatch { series_id, frames }));
            }

            tokio::time::sleep(Duration::from_millis(cfg.poll_period_ms)).await;
        }
    });

    rx
}

/// Fetch detector and filewriter config parameters for a new series, concurrently.
async fn fetch_series_metadata(client: &reqwest::Client, cfg: &MonitorConfig) -> FrameMetadata {
    let url_bcx = cfg.detector_config_url("beam_center_x");
    let url_bcy = cfg.detector_config_url("beam_center_y");
    let url_dist = cfg.detector_config_url("detector_distance");
    let url_wl = cfg.detector_config_url("wavelength");
    let url_energy = cfg.detector_config_url("incident_energy");
    let url_psx = cfg.detector_config_url("x_pixel_size");
    let url_psy = cfg.detector_config_url("y_pixel_size");
    let url_nimages = cfg.detector_config_url("nimages");
    let url_frame_time = cfg.detector_config_url("frame_time");
    let url_name_pattern = cfg.filewriter_config_url("name_pattern");

    let (bcx, bcy, dist, wl, energy, psx, psy, nimages, frame_time, name_pattern) = tokio::join!(
        fetch_config_f64(client, &url_bcx),
        fetch_config_f64(client, &url_bcy),
        fetch_config_f64(client, &url_dist),
        fetch_config_f64(client, &url_wl),
        fetch_config_f64(client, &url_energy),
        fetch_config_f64(client, &url_psx),
        fetch_config_f64(client, &url_psy),
        fetch_config_u32(client, &url_nimages),
        fetch_config_f64(client, &url_frame_time),
        fetch_config_string(client, &url_name_pattern),
    );
    FrameMetadata {
        beam_center_x: bcx.ok(),
        beam_center_y: bcy.ok(),
        detector_distance: dist.ok(),
        wavelength: wl.ok().filter(|&v| v > 0.0),
        incident_energy: energy.ok(),
        pixel_size_x: psx.ok(),
        pixel_size_y: psy.ok(),
        nimages: nimages.ok(),
        frame_time: frame_time.ok(),
        name_pattern: name_pattern.ok(),
        ..FrameMetadata::default()
    }
}

async fn fetch_config_f64(client: &reqwest::Client, url: &str) -> Result<f64> {
    let json: serde_json::Value =
        client.get(url).send().await?.error_for_status()?.json().await?;
    json["value"].as_f64().context("value not f64")
}

async fn fetch_config_u32(client: &reqwest::Client, url: &str) -> Result<u32> {
    let json: serde_json::Value =
        client.get(url).send().await?.error_for_status()?.json().await?;
    json["value"].as_u64().map(|v| v as u32).context("value not uint")
}

async fn fetch_config_string(client: &reqwest::Client, url: &str) -> Result<String> {
    let json: serde_json::Value =
        client.get(url).send().await?.error_for_status()?.json().await?;
    json["value"].as_str().map(str::to_owned).context("value not string")
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_decode_rejects_garbage() {
        assert!(decode_tiff(b"not a tiff").is_err());
    }
}
