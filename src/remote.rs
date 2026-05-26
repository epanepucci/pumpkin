use std::path::PathBuf;

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::net::TcpListener;
use tokio::sync::mpsc;

pub struct RemoteCmd {
    pub file: PathBuf,
    pub frame: usize,
}

/// Start a TCP listener on `port` that accepts newline-delimited JSON commands
/// of the form `{"file": "/path/to/master.h5", "frame": 42}` and forwards
/// them to the returned channel receiver.
pub fn start_remote_listener(port: u16) -> mpsc::UnboundedReceiver<RemoteCmd> {
    let (tx, rx) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        let addr = format!("0.0.0.0:{port}");
        let listener = match TcpListener::bind(&addr).await {
            Ok(l) => {
                eprintln!("Remote control listening on {addr}");
                l
            }
            Err(e) => {
                eprintln!("Remote control: bind failed on {addr}: {e}");
                return;
            }
        };
        loop {
            let Ok((stream, peer)) = listener.accept().await else {
                continue;
            };
            let tx = tx.clone();
            tokio::spawn(async move {
                let reader = BufReader::new(stream);
                let mut lines = reader.lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let line = line.trim().to_string();
                    if line.is_empty() {
                        continue;
                    }
                    match parse_cmd(&line) {
                        Some(cmd) => {
                            let _ = tx.send(cmd);
                        }
                        None => eprintln!("Remote: unrecognised command from {peer}: {line}"),
                    }
                }
            });
        }
    });
    rx
}

fn parse_cmd(line: &str) -> Option<RemoteCmd> {
    #[derive(serde::Deserialize)]
    struct Raw {
        file: String,
        frame: usize,
    }
    let raw: Raw = serde_json::from_str(line).ok()?;
    Some(RemoteCmd {
        file: PathBuf::from(raw.file),
        frame: raw.frame,
    })
}
