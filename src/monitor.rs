use anyhow::{Context, Result};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;

use crate::frame::Frame;
use crate::tiff_loader::decode_tiff;

/// Configuration for the monitor polling task.
#[derive(Clone, Debug)]
pub struct MonitorConfig {
    /// Base URL of the DCU, e.g. "http://192.168.1.1".
    pub dcu_url: String,
    /// SIMPLON API version string, e.g. "1.8.0".
    pub api_version: String,
    /// Long-poll timeout sent to the detector (milliseconds).
    pub poll_timeout_ms: u32,
}

impl MonitorConfig {
    pub fn monitor_url(&self) -> String {
        format!("{}/monitor/api/{}/images/monitor?timeout={}", self.dcu_url, self.api_version, self.poll_timeout_ms)
    }

    pub fn config_url(&self, param: &str) -> String {
        format!("{}/monitor/api/{}/config/{}", self.dcu_url, self.api_version, param)
    }
}

#[allow(dead_code)]
/// Enable the monitor interface on the detector.
///
/// Must be called before polling for images.
pub fn enable_monitor(client: &reqwest::blocking::Client, cfg: &MonitorConfig) -> Result<()> {
    let url = cfg.config_url("mode");
    let body = serde_json::json!({ "value": "enabled" });
    let resp = client.put(&url).json(&body).send().context("PUT monitor mode")?;
    if !resp.status().is_success() {
        anyhow::bail!("Enable monitor failed: HTTP {}", resp.status());
    }
    Ok(())
}

/// Spawn a background tokio task that polls the monitor endpoint and sends
/// decoded frames over a `watch` channel. Returns the receiver.
///
/// The task runs until the receiver is dropped.
pub fn start_monitor_task(cfg: MonitorConfig) -> watch::Receiver<Option<Arc<Frame>>> {
    let (tx, rx) = watch::channel(None);

    tokio::spawn(async move {
        let client = match reqwest::Client::builder()
            .timeout(Duration::from_millis(cfg.poll_timeout_ms as u64 + 2000))
            .build()
        {
            Ok(c) => c,
            Err(e) => {
                eprintln!("Monitor: failed to build HTTP client: {e}");
                return;
            }
        };

        // Enable monitor mode first.
        let enable_url = cfg.config_url("mode");
        let enable_body = serde_json::json!({ "value": "enabled" });
        if let Err(e) = client.put(&enable_url).json(&enable_body).send().await {
            eprintln!("Monitor: could not enable monitor mode: {e}");
            // Continue anyway — detector might already be enabled.
        }

        loop {
            if tx.is_closed() {
                break;
            }

            let url = cfg.monitor_url();
            match client.get(&url).send().await {
                Err(e) => {
                    eprintln!("Monitor poll error: {e}");
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
                Ok(resp) => {
                    let status = resp.status();
                    if status == reqwest::StatusCode::REQUEST_TIMEOUT {
                        // 408: no image available yet, just retry.
                        continue;
                    }
                    if !status.is_success() {
                        eprintln!("Monitor: unexpected HTTP {status}");
                        tokio::time::sleep(Duration::from_secs(1)).await;
                        continue;
                    }

                    match resp.bytes().await {
                        Err(e) => eprintln!("Monitor: reading body: {e}"),
                        Ok(bytes) => match decode_tiff(&bytes) {
                            Err(e) => eprintln!("Monitor: TIFF decode error: {e}"),
                            Ok(frame) => {
                                let _ = tx.send(Some(Arc::new(frame)));
                            }
                        },
                    }
                }
            }
        }
    });

    rx
}
