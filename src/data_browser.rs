use std::path::{Path, PathBuf};
use std::sync::mpsc;

use hdf5_metno as hdf5;

const BASE_PATH: &str = "/data/visitors/biomax";

// ─── Proposal cache ───────────────────────────────────────────────────────────

#[derive(serde::Serialize, serde::Deserialize)]
struct ProposalCache {
    timestamp_secs: u64,
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

fn save_proposal_cache(proposals: &[String]) {
    let Some(path) = cache_path() else { return };
    if let Some(p) = path.parent() {
        let _ = std::fs::create_dir_all(p);
    }
    let cache = ProposalCache { timestamp_secs: unix_now(), proposals: proposals.to_vec() };
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

/// Blocking: runs `id -nG`, extracts groups matching `\d{8}-group`.
fn fetch_proposals_from_os() -> Vec<String> {
    let Ok(out) = std::process::Command::new("id").arg("-nG").output() else {
        return vec![];
    };
    String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .filter_map(|g| {
            let prefix = g.strip_suffix("-group")?;
            (prefix.len() == 8 && prefix.bytes().all(|b| b.is_ascii_digit()))
                .then(|| prefix.to_string())
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
    /// Check the receiver for a result. Returns true if the state changed.
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

fn is_date_dir(name: &str) -> bool {
    name.len() == 8 && name.bytes().all(|b| b.is_ascii_digit())
}

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

/// Recursively collect `*_master.h5` files up to `depth` subdirectory levels.
fn find_master_files(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.filter_map(|e| e.ok()) {
        let path = entry.path();
        let Ok(ft) = entry.file_type() else { continue };
        if ft.is_file() {
            if path.file_name().and_then(|n| n.to_str())
                .is_some_and(|n| n.ends_with("_master.h5"))
            {
                out.push(path);
            }
        } else if ft.is_dir() && depth > 0 {
            find_master_files(&path, depth - 1, out);
        }
    }
}

// ─── Dataset metadata ─────────────────────────────────────────────────────────

#[derive(Clone, Default)]
pub struct DatasetMeta {
    pub ntrigger: Option<u32>,
    pub nimages:  Option<u32>,
    pub total:    Option<u32>,
    pub exp_ms:   Option<f64>,
}

fn read_meta(path: &Path) -> DatasetMeta {
    let Ok(f) = hdf5::File::open(path) else { return DatasetMeta::default() };

    let ntrigger = rd_u32(&f, "/entry/instrument/detector/detectorSpecific/ntrigger");
    let nimages  = rd_u32(&f, "/entry/instrument/detector/detectorSpecific/nimages");
    let count_t  = rd_f64(&f, "/entry/instrument/detector/count_time");
    let total    = ntrigger.zip(nimages).map(|(t, n)| t * n).or(nimages);

    DatasetMeta { ntrigger, nimages, total, exp_ms: count_t.map(|t| t * 1e3) }
}

fn rd_u32(f: &hdf5::File, path: &str) -> Option<u32> {
    let ds = f.dataset(path).ok()?;
    if let Ok(v) = ds.read_scalar::<u32>() { return Some(v); }
    if let Ok(v) = ds.read_scalar::<i32>() { return Some(v.max(0) as u32); }
    if let Ok(v) = ds.read_scalar::<i64>() { return Some(v.max(0) as u32); }
    if let Ok(v) = ds.read_scalar::<u64>() { return Some(v.min(u32::MAX as u64) as u32); }
    None
}

fn rd_f64(f: &hdf5::File, path: &str) -> Option<f64> {
    let ds = f.dataset(path).ok()?;
    if let Ok(v) = ds.read_scalar::<f64>() { return Some(v); }
    if let Ok(v) = ds.read_scalar::<f32>() { return Some(v as f64); }
    None
}

// ─── Tree nodes ───────────────────────────────────────────────────────────────

struct DatasetNode {
    name: String,
    path: PathBuf,
    meta: Option<DatasetMeta>,
}

struct ProteinNode {
    name: String,
    raw_path: PathBuf,
    open: bool,
    datasets: Async<DatasetList>,
}

struct DatasetList {
    nodes: Vec<DatasetNode>,
    meta_rx: Option<mpsc::Receiver<(PathBuf, DatasetMeta)>>,
    pending_meta: usize,
}

struct VisitNode {
    date: String,
    visit_path: PathBuf,
    open: bool,
    proteins: Async<Vec<ProteinNode>>,
    file_filter: String,
}

struct ProposalNode {
    number: String,
    open: bool,
    visits: Async<Vec<VisitNode>>,
    visit_filter: String,
}

// ─── Background loaders ───────────────────────────────────────────────────────

fn bg_load_visits(proposal: String) -> Result<Vec<VisitNode>, String> {
    let root = PathBuf::from(BASE_PATH).join(&proposal);
    let entries = std::fs::read_dir(&root)
        .map_err(|e| format!("Cannot list {}: {e}", root.display()))?;
    let mut visits: Vec<VisitNode> = entries
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            is_date_dir(&name).then(|| VisitNode {
                date: name,
                visit_path: e.path(),
                open: false,
                proteins: Async::Idle,
                file_filter: String::new(),
            })
        })
        .collect();
    visits.sort_by(|a, b| b.date.cmp(&a.date)); // most recent first
    Ok(visits)
}

fn bg_load_proteins(visit_path: PathBuf) -> Result<Vec<ProteinNode>, String> {
    let raw = visit_path.join("raw");
    let dirs = list_subdirs(&raw)
        .map_err(|e| format!("raw/: {e}"))?;
    let proteins = dirs.into_iter().map(|p| {
        let name = p.file_name().unwrap_or_default().to_string_lossy().into_owned();
        ProteinNode { name, raw_path: p, open: false, datasets: Async::Idle }
    }).collect();
    Ok(proteins)
}

fn bg_load_datasets(protein_path: PathBuf) -> Result<DatasetList, String> {
    let mut masters = Vec::new();
    find_master_files(&protein_path, 2, &mut masters);
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
    let nodes = masters.into_iter().map(|path| {
        let name = path.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .trim_end_matches("_master.h5")
            .to_string();
        DatasetNode { name, path, meta: None }
    }).collect();
    Ok(DatasetList {
        nodes,
        meta_rx: (pending_meta > 0).then_some(rx),
        pending_meta,
    })
}

// ─── Poll ─────────────────────────────────────────────────────────────────────

impl ProposalNode {
    fn poll(&mut self) -> bool {
        let mut changed = self.visits.poll();
        if let Async::Done(visits) = &mut self.visits {
            for v in visits.iter_mut() { changed |= v.poll(); }
        }
        changed
    }
    fn is_loading(&self) -> bool {
        self.visits.is_loading()
            || matches!(&self.visits, Async::Done(vs) if vs.iter().any(|v| v.is_loading()))
    }
}

impl VisitNode {
    fn poll(&mut self) -> bool {
        let mut changed = self.proteins.poll();
        if let Async::Done(proteins) = &mut self.proteins {
            for p in proteins.iter_mut() { changed |= p.poll(); }
        }
        changed
    }
    fn is_loading(&self) -> bool {
        self.proteins.is_loading()
            || matches!(&self.proteins, Async::Done(ps) if ps.iter().any(|p| p.is_loading()))
    }
}

impl ProteinNode {
    fn poll(&mut self) -> bool {
        let mut changed = self.datasets.poll();
        if let Async::Done(datasets) = &mut self.datasets {
            changed |= datasets.poll_meta();
        }
        changed
    }
    fn is_loading(&self) -> bool {
        self.datasets.is_loading()
            || matches!(&self.datasets, Async::Done(datasets) if datasets.is_loading())
    }
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

// ─── UI rendering ─────────────────────────────────────────────────────────────

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

fn tree_row(ui: &mut egui::Ui, open: bool, label: &str) -> bool {
    let icon = if open { "▼" } else { "▶" };
    let response = ui.selectable_label(false, format!("{icon} {label}"));
    response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, label));
    response.clicked()
}

fn loading_row(ui: &mut egui::Ui) {
    ui.horizontal(|ui| { ui.spinner(); ui.label("Loading…"); });
}

impl ProteinNode {
    fn show(&mut self, ui: &mut egui::Ui, file_filter: &str) -> Option<PathBuf> {
        if tree_row(ui, self.open, &self.name) {
            self.open = !self.open;
            if self.open && self.datasets.is_idle() {
                let path = self.raw_path.clone();
                self.datasets = spawn_load(move || bg_load_datasets(path));
            }
        }

        let mut action = None;
        if self.open {
            ui.indent(&self.raw_path, |ui| {
                match &mut self.datasets {
                    Async::Idle    => {}
                    Async::Loading(_) => loading_row(ui),
                    Async::Failed(e) => {
                        let resp = ui.label(egui::RichText::new(format!("⚠ {e}")).small());
                        resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Other, format!("Error: {e}")));
                    }
                    Async::Done(datasets) => {
                        if datasets.nodes.is_empty() {
                            ui.label(egui::RichText::new("No datasets found").small().weak());
                        }
                        for ds in datasets.nodes.iter() {
                            if filter_matches(&ds.name, file_filter) {
                                if ds.show(ui) { action = Some(ds.path.clone()); }
                            }
                        }
                    }
                }
            });
        }
        action
    }
}

impl VisitNode {
    fn show(&mut self, ui: &mut egui::Ui) -> Option<PathBuf> {
        if tree_row(ui, self.open, &self.date) {
            self.open = !self.open;
            if self.open && self.proteins.is_idle() {
                let path = self.visit_path.clone();
                self.proteins = spawn_load(move || bg_load_proteins(path));
            }
        }

        let mut action = None;
        if self.open {
            ui.indent(&self.visit_path, |ui| {
                filter_bar(ui, &mut self.file_filter, "Filter filenames…");
                let file_filter = self.file_filter.to_lowercase();
                match &mut self.proteins {
                    Async::Idle    => {}
                    Async::Loading(_) => loading_row(ui),
                    Async::Failed(e) => {
                        let resp = ui.label(egui::RichText::new(format!("⚠ {e}")).small());
                        resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Other, format!("Error: {e}")));
                    }
                    Async::Done(proteins) => {
                        if proteins.is_empty() {
                            ui.label(egui::RichText::new("No proteins found").small().weak());
                        }
                        for p in proteins.iter_mut() {
                            if let Some(path) = p.show(ui, &file_filter) { action = Some(path); }
                        }
                    }
                }
            });
        }
        action
    }
}

impl ProposalNode {
    fn show(&mut self, ui: &mut egui::Ui) -> Option<PathBuf> {
        if tree_row(ui, self.open, &self.number) {
            self.open = !self.open;
            if self.open && self.visits.is_idle() {
                let number = self.number.clone();
                self.visits = spawn_load(move || bg_load_visits(number));
            }
        }

        let mut action = None;
        if self.open {
            ui.indent(&self.number, |ui| {
                filter_bar(ui, &mut self.visit_filter, "Filter dates…");
                let visit_filter = self.visit_filter.to_lowercase();
                match &mut self.visits {
                    Async::Idle    => {}
                    Async::Loading(_) => loading_row(ui),
                    Async::Failed(e) => {
                        let resp = ui.label(egui::RichText::new(format!("⚠ {e}")).small());
                        resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Other, format!("Error: {e}")));
                    }
                    Async::Done(visits) => {
                        if visits.is_empty() {
                            ui.label(egui::RichText::new("No visits found").small().weak());
                        }
                        for v in visits.iter_mut() {
                            if filter_matches(&v.date, &visit_filter) {
                                if let Some(path) = v.show(ui) { action = Some(path); }
                            }
                        }
                    }
                }
            });
        }
        action
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
}

impl DataBrowser {
    pub fn new() -> Self {
        let state = if let Some(cache) = load_proposal_cache() {
            if cache_is_fresh(&cache) {
                RootState::Ready(Self::make_proposal_nodes(&cache.proposals))
            } else {
                Self::start_group_fetch()
            }
        } else {
            Self::start_group_fetch()
        };
        Self { state, proposal_filter: String::new() }
    }

    fn make_proposal_nodes(proposals: &[String]) -> Vec<ProposalNode> {
        let mut nodes: Vec<ProposalNode> = proposals.iter().map(|p| ProposalNode {
            number: p.clone(),
            open: false,
            visits: Async::Idle,
            visit_filter: String::new(),
        }).collect();
        nodes.sort_by(|a, b| b.number.cmp(&a.number));
        nodes
    }

    fn start_group_fetch() -> RootState {
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let proposals = fetch_proposals_from_os();
            save_proposal_cache(&proposals);
            let _ = tx.send(proposals);
        });
        RootState::FetchingGroups(rx)
    }

    /// True if any background load is in progress (caller should schedule repaint).
    pub fn is_loading(&self) -> bool {
        match &self.state {
            RootState::FetchingGroups(_) => true,
            RootState::Ready(proposals)  => proposals.iter().any(|p| p.is_loading()),
        }
    }

    /// Poll all in-flight loads. Returns true if any state changed.
    pub fn poll(&mut self) -> bool {
        match &mut self.state {
            RootState::FetchingGroups(rx) => match rx.try_recv() {
                Ok(proposals) => {
                    self.state = RootState::Ready(Self::make_proposal_nodes(&proposals));
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

    /// Render the browser. Returns `Some(path)` if the user clicked a dataset.
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
                let mut action = None;
                for proposal in proposals.iter_mut() {
                    if filter_matches(&proposal.number, &proposal_filter) {
                        if let Some(path) = proposal.show(ui) {
                            action = Some(path);
                        }
                    }
                }
                action
            }
        }
    }
}
