use std::path::{Path, PathBuf};
use std::sync::mpsc;

use hdf5_metno as hdf5;

use crate::config::{DataBrowserConfig, DatasetConfig, LevelConfig, ProposalSource};

// ─── Proposal cache ───────────────────────────────────────────────────────────

#[derive(serde::Serialize, serde::Deserialize)]
struct ProposalCache {
    timestamp_secs: u64,
    base_path: String,
    proposals: Vec<String>,
}

fn cache_path() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|h| {
        PathBuf::from(h)
            .join(".config")
            .join("pumpkin")
            .join("proposals_cache.json")
    })
}

fn load_proposal_cache() -> Option<ProposalCache> {
    let path = cache_path()?;
    let text = std::fs::read_to_string(&path).ok()?;
    serde_json::from_str(&text).ok()
}

fn save_proposal_cache(proposals: &[String], base_path: &str) {
    let Some(path) = cache_path() else { return };
    if let Some(p) = path.parent() {
        let _ = std::fs::create_dir_all(p);
    }
    let cache = ProposalCache {
        timestamp_secs: unix_now(),
        base_path: base_path.to_string(),
        proposals: proposals.to_vec(),
    };
    if let Ok(json) = serde_json::to_string(&cache) {
        let _ = std::fs::write(path, json);
    }
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn cache_is_fresh(cache: &ProposalCache) -> bool {
    unix_now().saturating_sub(cache.timestamp_secs) < 86_400
}

/// Blocking: runs `id -nG`, extracts proposal IDs from matching group names.
fn fetch_proposals_from_os(source: &ProposalSource) -> Vec<String> {
    let Ok(out) = std::process::Command::new("id").arg("-nG").output() else {
        return vec![];
    };
    String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .filter_map(|g| {
            let prefix = g.strip_suffix(&source.group_suffix)?;
            if source.proposal_id_digits > 0 {
                (prefix.len() == source.proposal_id_digits
                    && prefix.bytes().all(|b| b.is_ascii_digit()))
                .then(|| prefix.to_string())
            } else {
                (!prefix.is_empty()).then(|| prefix.to_string())
            }
        })
        .collect()
}

// ─── Async load state ─────────────────────────────────────────────────────────

enum Async<T> {
    Idle,
    Loading(mpsc::Receiver<Result<T, String>>),
    Done(T),
    Failed(String),
}

impl<T> Async<T> {
    fn poll(&mut self) -> bool {
        let prev = std::mem::replace(self, Async::Idle);
        match prev {
            Async::Loading(rx) => match rx.try_recv() {
                Ok(Ok(v))  => { *self = Async::Done(v);           true  }
                Ok(Err(e)) => { *self = Async::Failed(e);         true  }
                Err(mpsc::TryRecvError::Empty) => {
                    *self = Async::Loading(rx);                    false
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    *self = Async::Failed("thread disconnected".into()); true
                }
            },
            other => { *self = other; false }
        }
    }

    fn is_loading(&self) -> bool { matches!(self, Async::Loading(_)) }
    fn is_idle(&self)    -> bool { matches!(self, Async::Idle) }
}

fn spawn_load<T, F>(f: F) -> Async<T>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, String> + Send + 'static,
{
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || { let _ = tx.send(f()); });
    Async::Loading(rx)
}

// ─── Filesystem helpers ───────────────────────────────────────────────────────

fn list_subdirs(path: &Path) -> Result<Vec<PathBuf>, String> {
    let entries = std::fs::read_dir(path)
        .map_err(|e| format!("Cannot list {}: {e}", path.display()))?;
    let mut dirs: Vec<PathBuf> = entries
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .map(|e| e.path())
        .collect();
    dirs.sort();
    Ok(dirs)
}

/// Recursively collect files whose names end with `suffix` up to `depth` levels.
fn find_master_files(dir: &Path, depth: usize, suffix: &str, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.filter_map(|e| e.ok()) {
        let path = entry.path();
        let Ok(ft) = entry.file_type() else { continue };
        if ft.is_file() {
            if path.file_name().and_then(|n| n.to_str())
                .is_some_and(|n| n.ends_with(suffix))
            {
                out.push(path);
            }
        } else if ft.is_dir() && depth > 0 {
            find_master_files(&path, depth - 1, suffix, out);
        }
    }
}

// ─── Dataset metadata ─────────────────────────────────────────────────────────

#[derive(Clone, Default)]
pub struct DatasetMeta {
    pub ntrigger: Option<u64>,
    pub nimages:  Option<u64>,
    pub total:    Option<u64>,
    pub exp_ms:   Option<f64>,
}

fn read_meta(path: &Path) -> DatasetMeta {
    let Ok(f) = hdf5::File::open(path) else { return DatasetMeta::default() };

    let ntrigger = rd_u64(&f, "/entry/instrument/detector/detectorSpecific/ntrigger");
    let nimages  = rd_u64(&f, "/entry/instrument/detector/detectorSpecific/nimages");
    let count_t  = rd_f64(&f, "/entry/instrument/detector/count_time");
    let total    = ntrigger.zip(nimages).map(|(t, n)| t * n).or(nimages);

    DatasetMeta { ntrigger, nimages, total, exp_ms: count_t.map(|t| t * 1e3) }
}

fn rd_u64(f: &hdf5::File, path: &str) -> Option<u64> {
    let ds = f.dataset(path).ok()?;
    if let Ok(v) = ds.read_scalar::<u64>() { return Some(v); }
    if let Ok(v) = ds.read_scalar::<u32>() { return Some(v as u64); }
    if let Ok(v) = ds.read_scalar::<i64>() { return Some(v.max(0) as u64); }
    if let Ok(v) = ds.read_scalar::<i32>() { return Some(v.max(0) as u64); }
    None
}

fn rd_f64(f: &hdf5::File, path: &str) -> Option<f64> {
    let ds = f.dataset(path).ok()?;
    if let Ok(v) = ds.read_scalar::<f64>() { return Some(v); }
    if let Ok(v) = ds.read_scalar::<f32>() { return Some(v as f64); }
    None
}

// ─── Background loaders ───────────────────────────────────────────────────────

/// Load one level of directory nodes under `parent_path`, applying `level_cfg`.
fn bg_load_level(parent_path: PathBuf, level_cfg: LevelConfig) -> Result<Vec<TreeNode>, String> {
    let search_root = if level_cfg.subdir.is_empty() {
        parent_path.clone()
    } else {
        parent_path.join(&level_cfg.subdir)
    };

    let dirs = list_subdirs(&search_root)?;

    let mut nodes: Vec<TreeNode> = dirs
        .into_iter()
        .filter(|p| {
            if !level_cfg.date_only {
                return true;
            }
            p.file_name()
                .and_then(|n| n.to_str())
                .map(|n| n.len() == level_cfg.date_dir_len && n.bytes().all(|b| b.is_ascii_digit()))
                .unwrap_or(false)
        })
        .map(|p| {
            let name = p.file_name().unwrap_or_default().to_string_lossy().into_owned();
            TreeNode {
                name,
                path: p,
                open: false,
                child_filter: String::new(),
                children: NodeChildren::Levels(Async::Idle),
            }
        })
        .collect();

    if level_cfg.sort_desc {
        nodes.sort_by(|a, b| b.name.cmp(&a.name));
    }
    // dirs are already sorted ascending from list_subdirs; only sort again for desc.

    Ok(nodes)
}

fn bg_load_datasets(leaf_path: PathBuf, cfg: DatasetConfig) -> Result<DatasetList, String> {
    let search_root = if cfg.subdir.is_empty() {
        leaf_path.clone()
    } else {
        leaf_path.join(&cfg.subdir)
    };

    let mut masters = Vec::new();
    find_master_files(&search_root, cfg.search_depth, &cfg.file_suffix, &mut masters);
    masters.sort();

    let (tx, rx) = mpsc::channel();
    let meta_paths = masters.clone();
    std::thread::spawn(move || {
        for path in meta_paths {
            let meta = read_meta(&path);
            if tx.send((path, meta)).is_err() {
                break;
            }
        }
    });

    let pending_meta = masters.len();
    let suffix = cfg.file_suffix.clone();
    let nodes = masters.into_iter().map(|path| {
        let name = path.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .trim_end_matches(suffix.as_str())
            .to_string();
        DatasetNode { name, path, meta: None }
    }).collect();

    Ok(DatasetList {
        nodes,
        meta_rx: (pending_meta > 0).then_some(rx),
        pending_meta,
    })
}

// ─── Tree nodes ───────────────────────────────────────────────────────────────

struct DatasetNode {
    name: String,
    path: PathBuf,
    meta: Option<DatasetMeta>,
}

struct DatasetList {
    nodes: Vec<DatasetNode>,
    meta_rx: Option<mpsc::Receiver<(PathBuf, DatasetMeta)>>,
    pending_meta: usize,
}

impl DatasetList {
    fn poll_meta(&mut self) -> bool {
        let Some(rx) = self.meta_rx.take() else { return false };
        let mut changed = false;

        loop {
            match rx.try_recv() {
                Ok((path, meta)) => {
                    if let Some(node) = self.nodes.iter_mut().find(|n| n.path == path) {
                        node.meta = Some(meta);
                        changed = true;
                    }
                    self.pending_meta = self.pending_meta.saturating_sub(1);
                }
                Err(mpsc::TryRecvError::Empty) => {
                    self.meta_rx = Some(rx);
                    break;
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.pending_meta = 0;
                    break;
                }
            }
        }

        changed
    }

    fn is_loading(&self) -> bool {
        self.pending_meta > 0 && self.meta_rx.is_some()
    }
}

enum NodeChildren {
    /// More directory levels remain below this node.
    Levels(Async<Vec<TreeNode>>),
    /// This is the leaf level; children are dataset files.
    Datasets(Async<DatasetList>),
}

struct TreeNode {
    name: String,
    path: PathBuf,
    open: bool,
    child_filter: String,
    children: NodeChildren,
}

impl TreeNode {
    /// Called when the node is first opened. `depth` is this node's position in
    /// `cfg.levels` (0 = first level below proposals). Children are at `depth + 1`.
    fn trigger_load(&mut self, depth: usize, cfg: &DataBrowserConfig) {
        let is_idle = match &self.children {
            NodeChildren::Levels(a) => a.is_idle(),
            NodeChildren::Datasets(a) => a.is_idle(),
        };
        if !is_idle {
            return;
        }

        let path = self.path.clone();
        let next = depth + 1;
        if next >= cfg.levels.len() {
            let ds_cfg = cfg.datasets.clone();
            self.children = NodeChildren::Datasets(spawn_load(move || bg_load_datasets(path, ds_cfg)));
        } else {
            let level_cfg = cfg.levels[next].clone();
            self.children = NodeChildren::Levels(spawn_load(move || bg_load_level(path, level_cfg)));
        }
    }

    fn poll(&mut self) -> bool {
        match &mut self.children {
            NodeChildren::Levels(a) => {
                let changed = a.poll();
                if let Async::Done(children) = a {
                    children.iter_mut().fold(changed, |acc, c| acc | c.poll())
                } else {
                    changed
                }
            }
            NodeChildren::Datasets(a) => {
                let changed = a.poll();
                if let Async::Done(ds) = a { ds.poll_meta() | changed } else { changed }
            }
        }
    }

    fn is_loading(&self) -> bool {
        match &self.children {
            NodeChildren::Levels(a) => {
                a.is_loading()
                    || matches!(a, Async::Done(cs) if cs.iter().any(|c| c.is_loading()))
            }
            NodeChildren::Datasets(a) => {
                a.is_loading() || matches!(a, Async::Done(ds) if ds.is_loading())
            }
        }
    }

    fn show(&mut self, ui: &mut egui::Ui, depth: usize, cfg: &DataBrowserConfig) -> Option<PathBuf> {
        if tree_row(ui, self.open, &self.name) {
            self.open = !self.open;
            if self.open {
                self.trigger_load(depth, cfg);
            }
        }

        if !self.open {
            return None;
        }

        let mut action = None;
        ui.indent(&self.path, |ui| {
            // Filter bar: hint names what's being filtered (children of this node).
            let next = depth + 1;
            let hint = if next < cfg.levels.len() {
                format!("Filter {}…", cfg.levels[next].label)
            } else {
                "Filter files…".to_string()
            };
            filter_bar(ui, &mut self.child_filter, &hint);
            let filter = self.child_filter.to_lowercase();

            match &mut self.children {
                NodeChildren::Levels(a) => match a {
                    Async::Idle => {}
                    Async::Loading(_) => loading_row(ui),
                    Async::Failed(e) => error_row(ui, e),
                    Async::Done(children) => {
                        if children.is_empty() {
                            let lbl = cfg.levels.get(next).map(|l| l.label.as_str()).unwrap_or("entries");
                            ui.label(egui::RichText::new(format!("No {lbl} found")).small().weak());
                        }
                        for child in children.iter_mut() {
                            if filter_matches(&child.name, &filter) {
                                if let Some(p) = child.show(ui, next, cfg) {
                                    action = Some(p);
                                }
                            }
                        }
                    }
                },
                NodeChildren::Datasets(a) => match a {
                    Async::Idle => {}
                    Async::Loading(_) => loading_row(ui),
                    Async::Failed(e) => error_row(ui, e),
                    Async::Done(datasets) => {
                        if datasets.nodes.is_empty() {
                            ui.label(egui::RichText::new("No datasets found").small().weak());
                        }
                        for ds in datasets.nodes.iter() {
                            if filter_matches(&ds.name, &filter) {
                                if ds.show(ui) {
                                    action = Some(ds.path.clone());
                                }
                            }
                        }
                    }
                },
            }
        });
        action
    }
}

struct ProposalNode {
    number: String,
    open: bool,
    child_filter: String,
    children: NodeChildren,
}

impl ProposalNode {
    fn trigger_load(&mut self, cfg: &DataBrowserConfig) {
        let is_idle = match &self.children {
            NodeChildren::Levels(a) => a.is_idle(),
            NodeChildren::Datasets(a) => a.is_idle(),
        };
        if !is_idle {
            return;
        }

        let path = PathBuf::from(&cfg.proposal_source.base_path).join(&self.number);
        if cfg.levels.is_empty() {
            let ds_cfg = cfg.datasets.clone();
            self.children = NodeChildren::Datasets(spawn_load(move || bg_load_datasets(path, ds_cfg)));
        } else {
            let level_cfg = cfg.levels[0].clone();
            self.children = NodeChildren::Levels(spawn_load(move || bg_load_level(path, level_cfg)));
        }
    }

    fn poll(&mut self) -> bool {
        match &mut self.children {
            NodeChildren::Levels(a) => {
                let changed = a.poll();
                if let Async::Done(children) = a {
                    children.iter_mut().fold(changed, |acc, c| acc | c.poll())
                } else {
                    changed
                }
            }
            NodeChildren::Datasets(a) => {
                let changed = a.poll();
                if let Async::Done(ds) = a { ds.poll_meta() | changed } else { changed }
            }
        }
    }

    fn is_loading(&self) -> bool {
        match &self.children {
            NodeChildren::Levels(a) => {
                a.is_loading()
                    || matches!(a, Async::Done(cs) if cs.iter().any(|c| c.is_loading()))
            }
            NodeChildren::Datasets(a) => {
                a.is_loading() || matches!(a, Async::Done(ds) if ds.is_loading())
            }
        }
    }

    fn show(&mut self, ui: &mut egui::Ui, cfg: &DataBrowserConfig) -> Option<PathBuf> {
        if tree_row(ui, self.open, &self.number) {
            self.open = !self.open;
            if self.open {
                self.trigger_load(cfg);
            }
        }

        if !self.open {
            return None;
        }

        let mut action = None;
        ui.indent(&self.number, |ui| {
            let hint = if cfg.levels.is_empty() {
                "Filter files…".to_string()
            } else {
                format!("Filter {}…", cfg.levels[0].label)
            };
            filter_bar(ui, &mut self.child_filter, &hint);
            let filter = self.child_filter.to_lowercase();

            match &mut self.children {
                NodeChildren::Levels(a) => match a {
                    Async::Idle => {}
                    Async::Loading(_) => loading_row(ui),
                    Async::Failed(e) => error_row(ui, e),
                    Async::Done(children) => {
                        if children.is_empty() {
                            let lbl = cfg.levels.first().map(|l| l.label.as_str()).unwrap_or("entries");
                            ui.label(egui::RichText::new(format!("No {lbl} found")).small().weak());
                        }
                        for child in children.iter_mut() {
                            if filter_matches(&child.name, &filter) {
                                if let Some(p) = child.show(ui, 0, cfg) {
                                    action = Some(p);
                                }
                            }
                        }
                    }
                },
                NodeChildren::Datasets(a) => match a {
                    Async::Idle => {}
                    Async::Loading(_) => loading_row(ui),
                    Async::Failed(e) => error_row(ui, e),
                    Async::Done(datasets) => {
                        if datasets.nodes.is_empty() {
                            ui.label(egui::RichText::new("No datasets found").small().weak());
                        }
                        for ds in datasets.nodes.iter() {
                            if filter_matches(&ds.name, &filter) {
                                if ds.show(ui) {
                                    action = Some(ds.path.clone());
                                }
                            }
                        }
                    }
                },
            }
        });
        action
    }
}

// ─── UI helpers ───────────────────────────────────────────────────────────────

fn filter_matches(label: &str, filter: &str) -> bool {
    filter.is_empty() || label.to_lowercase().contains(filter)
}

fn filter_bar(ui: &mut egui::Ui, value: &mut String, hint: &str) {
    ui.add_space(2.0);
    // Capture available_width from the *vertical* layout context — inside a
    // ui.horizontal() the max_rect is infinite in x, making available_width()
    // unreliable.  add_sized then forces the TextEdit to exactly this width so
    // the horizontal layout never reports a wider min_rect than the panel.
    let total_w = ui.available_width();
    let btn_w = ui.spacing().interact_size.x + ui.spacing().item_spacing.x;
    let edit_h = ui.spacing().interact_size.y;
    ui.horizontal(|ui| {
        ui.add_sized(
            egui::vec2((total_w - btn_w).max(20.0), edit_h),
            egui::TextEdit::singleline(value)
                .hint_text(hint)
                .font(egui::TextStyle::Small),
        );
        if !value.is_empty() && ui.button("✕").on_hover_text("Clear filter").clicked() {
            value.clear();
        }
    });
    ui.add_space(2.0);
}

fn tree_row(ui: &mut egui::Ui, open: bool, label: &str) -> bool {
    let icon = if open { "▼" } else { "▶" };
    let response = ui.selectable_label(false, format!("{icon} {label}"));
    response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, label));
    response.clicked()
}

fn loading_row(ui: &mut egui::Ui) {
    ui.horizontal(|ui| { ui.spinner(); ui.label("Loading…"); });
}

fn error_row(ui: &mut egui::Ui, e: &str) {
    let resp = ui.label(egui::RichText::new(format!("⚠ {e}")).small());
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Other, true, format!("Error: {e}")));
}

impl DatasetNode {
    fn show(&self, ui: &mut egui::Ui) -> bool {
        let response = ui.add(
            egui::Label::new(egui::RichText::new(&self.name).monospace().small())
                .sense(egui::Sense::click()),
        );
        match &self.meta {
            Some(meta) => {
                let mut parts = Vec::<String>::new();
                if let Some(n) = meta.ntrigger { parts.push(format!("ntrigger:{n}")); }
                if let Some(n) = meta.nimages  { parts.push(format!("nimages:{n}")); }
                if let Some(t) = meta.total    { parts.push(format!("total:{t}")); }
                if let Some(e) = meta.exp_ms   { parts.push(format!("exp:{:.1}ms", e)); }
                if !parts.is_empty() {
                    ui.label(egui::RichText::new(parts.join("  ")).small().weak());
                }
            }
            None => {
                ui.label(egui::RichText::new("Reading metadata…").small().weak());
            }
        }
        response.clicked()
    }
}

// ─── DataBrowser ──────────────────────────────────────────────────────────────

enum RootState {
    FetchingGroups(mpsc::Receiver<Vec<String>>),
    Ready(Vec<ProposalNode>),
}

pub struct DataBrowser {
    state: RootState,
    proposal_filter: String,
    cfg: DataBrowserConfig,
}

impl DataBrowser {
    pub fn new(cfg: DataBrowserConfig) -> Self {
        let source = &cfg.proposal_source;
        let state = if let Some(cache) = load_proposal_cache() {
            if cache_is_fresh(&cache) && cache.base_path == source.base_path {
                RootState::Ready(Self::make_proposal_nodes(&cache.proposals, &cfg))
            } else {
                Self::start_group_fetch(source.clone())
            }
        } else {
            Self::start_group_fetch(source.clone())
        };
        Self { state, proposal_filter: String::new(), cfg }
    }

    fn make_proposal_nodes(proposals: &[String], cfg: &DataBrowserConfig) -> Vec<ProposalNode> {
        let levels_empty = cfg.levels.is_empty();
        let mut nodes: Vec<ProposalNode> = proposals.iter().map(|p| ProposalNode {
            number: p.clone(),
            open: false,
            child_filter: String::new(),
            children: if levels_empty {
                NodeChildren::Datasets(Async::Idle)
            } else {
                NodeChildren::Levels(Async::Idle)
            },
        }).collect();
        nodes.sort_by(|a, b| b.number.cmp(&a.number));
        nodes
    }

    fn start_group_fetch(source: ProposalSource) -> RootState {
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let proposals = fetch_proposals_from_os(&source);
            save_proposal_cache(&proposals, &source.base_path);
            let _ = tx.send(proposals);
        });
        RootState::FetchingGroups(rx)
    }

    pub fn is_loading(&self) -> bool {
        match &self.state {
            RootState::FetchingGroups(_) => true,
            RootState::Ready(proposals)  => proposals.iter().any(|p| p.is_loading()),
        }
    }

    pub fn poll(&mut self) -> bool {
        match &mut self.state {
            RootState::FetchingGroups(rx) => match rx.try_recv() {
                Ok(proposals) => {
                    self.state = RootState::Ready(Self::make_proposal_nodes(&proposals, &self.cfg));
                    true
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.state = RootState::Ready(vec![]);
                    true
                }
                Err(mpsc::TryRecvError::Empty) => false,
            },
            RootState::Ready(proposals) => {
                proposals.iter_mut().fold(false, |acc, p| acc | p.poll())
            }
        }
    }

    pub fn show(&mut self, ui: &mut egui::Ui) -> Option<PathBuf> {
        filter_bar(ui, &mut self.proposal_filter, "Filter proposals…");
        let proposal_filter = self.proposal_filter.to_lowercase();

        match &mut self.state {
            RootState::FetchingGroups(_) => {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label("Looking up proposals…");
                });
                None
            }
            RootState::Ready(proposals) => {
                if proposals.is_empty() {
                    ui.label(egui::RichText::new("No proposals found.").weak());
                    return None;
                }
                let cfg = &self.cfg;
                let mut action = None;
                for proposal in proposals.iter_mut() {
                    if filter_matches(&proposal.number, &proposal_filter) {
                        if let Some(path) = proposal.show(ui, cfg) {
                            action = Some(path);
                        }
                    }
                }
                action
            }
        }
    }
}
