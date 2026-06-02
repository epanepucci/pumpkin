use std::path::PathBuf;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};

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

pub(crate) fn parse_cmd(line: &str) -> Option<RemoteCmd> {
    #[derive(serde::Deserialize)]
    struct Raw {
        file: String,
        frame: usize,
    }
    let raw: Raw = serde_json::from_str(line).ok()?;
    Some(RemoteCmd { file: PathBuf::from(raw.file), frame: raw.frame })
}

pub struct CommandsFileConfig {
    pub path: PathBuf,
    pub poll_interval: Duration,
}

pub struct CommandsFileWatcher {
    stop: Arc<AtomicBool>,
    join: Option<std::thread::JoinHandle<()>>,
}

impl CommandsFileWatcher {
    pub fn stop(mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

impl Drop for CommandsFileWatcher {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

pub fn start_commands_file_watcher(
    cfg: CommandsFileConfig,
) -> (CommandsFileWatcher, mpsc::UnboundedReceiver<RemoteCmd>) {
    let (tx, rx) = mpsc::unbounded_channel();
    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = stop.clone();
    let join = std::thread::spawn(move || {
        commands_file_thread(cfg, tx, thread_stop);
    });
    (CommandsFileWatcher { stop, join: Some(join) }, rx)
}

fn commands_file_thread(cfg: CommandsFileConfig, tx: mpsc::UnboundedSender<RemoteCmd>, stop: Arc<AtomicBool>) {
    let mut state = CommandsFileState::default();
    let poll_interval = cfg.poll_interval.max(Duration::from_millis(50));
    let mut inotify = InotifyHandle::new(&cfg.path);
    if inotify.is_some() {
        eprintln!("Commands file: using inotify + polling for {}", cfg.path.display());
    } else {
        eprintln!("Commands file: using polling for {}", cfg.path.display());
    }

    read_commands_file(&cfg.path, &mut state, &tx);
    let mut last_poll = Instant::now();

    while !stop.load(Ordering::Relaxed) {
        let event_seen = inotify.as_mut().is_some_and(|h| h.drain_events());
        if event_seen || last_poll.elapsed() >= poll_interval {
            read_commands_file(&cfg.path, &mut state, &tx);
            last_poll = Instant::now();
        }
        std::thread::sleep(Duration::from_millis(100).min(poll_interval));
    }
}

#[derive(Default)]
struct CommandsFileState {
    offset: u64,
    last_modified: Option<std::time::SystemTime>,
}

fn read_commands_file(path: &std::path::Path, state: &mut CommandsFileState, tx: &mpsc::UnboundedSender<RemoteCmd>) {
    use std::io::{Read, Seek, SeekFrom};

    let Ok(mut file) = std::fs::File::open(path) else {
        state.offset = 0;
        return;
    };
    let Ok(metadata) = file.metadata() else {
        return;
    };
    let modified = metadata.modified().ok();
    if metadata.len() < state.offset {
        state.offset = 0;
    } else if metadata.len() == state.offset {
        if modified == state.last_modified {
            return;
        }
        state.offset = 0;
    }
    if file.seek(SeekFrom::Start(state.offset)).is_err() {
        state.offset = 0;
        let _ = file.seek(SeekFrom::Start(0));
    }

    let mut text = String::new();
    if file.read_to_string(&mut text).is_err() {
        return;
    }
    state.offset = metadata.len();
    state.last_modified = modified;

    for line in text.lines().map(str::trim).filter(|line| !line.is_empty()) {
        match parse_cmd(line) {
            Some(cmd) => {
                let _ = tx.send(cmd);
            }
            None => eprintln!("Commands file: unrecognised command in {}: {line}", path.display()),
        }
    }
}

#[cfg(target_os = "linux")]
struct InotifyHandle {
    fd: std::os::raw::c_int,
}

#[cfg(target_os = "linux")]
impl InotifyHandle {
    fn new(path: &std::path::Path) -> Option<Self> {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;

        const IN_MODIFY: u32 = 0x0000_0002;
        const IN_CLOSE_WRITE: u32 = 0x0000_0008;
        const IN_MOVED_TO: u32 = 0x0000_0080;
        const IN_CREATE: u32 = 0x0000_0100;
        const IN_DELETE_SELF: u32 = 0x0000_0400;
        const IN_MOVE_SELF: u32 = 0x0000_0800;
        const IN_NONBLOCK: std::os::raw::c_int = 0x0000_0800;
        const IN_CLOEXEC: std::os::raw::c_int = 0x0008_0000;

        let watch_path = path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(path);
        let c_path = CString::new(watch_path.as_os_str().as_bytes()).ok()?;
        let fd = unsafe { inotify_init1(IN_NONBLOCK | IN_CLOEXEC) };
        if fd < 0 {
            return None;
        }
        let mask = IN_MODIFY | IN_CLOSE_WRITE | IN_MOVED_TO | IN_CREATE | IN_DELETE_SELF | IN_MOVE_SELF;
        let wd = unsafe { inotify_add_watch(fd, c_path.as_ptr(), mask) };
        if wd < 0 {
            unsafe {
                close(fd);
            }
            return None;
        }
        Some(Self { fd })
    }

    fn drain_events(&mut self) -> bool {
        let mut buf = [0_u8; 4096];
        let n = unsafe { read(self.fd, buf.as_mut_ptr().cast::<std::ffi::c_void>(), buf.len()) };
        n > 0
    }
}

#[cfg(target_os = "linux")]
impl Drop for InotifyHandle {
    fn drop(&mut self) {
        unsafe {
            close(self.fd);
        }
    }
}

#[cfg(target_os = "linux")]
unsafe extern "C" {
    fn inotify_init1(flags: std::os::raw::c_int) -> std::os::raw::c_int;
    fn inotify_add_watch(
        fd: std::os::raw::c_int,
        pathname: *const std::os::raw::c_char,
        mask: u32,
    ) -> std::os::raw::c_int;
    fn read(fd: std::os::raw::c_int, buf: *mut std::ffi::c_void, count: usize) -> isize;
    fn close(fd: std::os::raw::c_int) -> std::os::raw::c_int;
}

#[cfg(not(target_os = "linux"))]
struct InotifyHandle;

#[cfg(not(target_os = "linux"))]
impl InotifyHandle {
    fn new(_path: &std::path::Path) -> Option<Self> {
        None
    }

    fn drain_events(&mut self) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn commands_file_reads_appended_json_commands() {
        let path = std::env::temp_dir().join(format!("pumpkin-commands-file-test-{}.txt", std::process::id()));
        let _ = std::fs::remove_file(&path);
        std::fs::write(&path, "{\"file\":\"/tmp/a_master.h5\",\"frame\":1}\n").unwrap();

        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut state = CommandsFileState::default();
        read_commands_file(&path, &mut state, &tx);

        let cmd = rx.try_recv().unwrap();
        assert_eq!(cmd.file, PathBuf::from("/tmp/a_master.h5"));
        assert_eq!(cmd.frame, 1);
        assert!(rx.try_recv().is_err());

        read_commands_file(&path, &mut state, &tx);
        assert!(rx.try_recv().is_err());

        let mut file = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
        writeln!(file, "{{\"file\":\"/tmp/b_master.h5\",\"frame\":2}}").unwrap();
        drop(file);

        read_commands_file(&path, &mut state, &tx);
        let cmd = rx.try_recv().unwrap();
        assert_eq!(cmd.file, PathBuf::from("/tmp/b_master.h5"));
        assert_eq!(cmd.frame, 2);
        assert!(rx.try_recv().is_err());

        let _ = std::fs::remove_file(&path);
    }
}
