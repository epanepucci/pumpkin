use std::path::{Path, PathBuf};

use serde::Deserialize;

// ─── JSON deserialization ─────────────────────────────────────────────────────

#[derive(Deserialize)]
struct RawFrame {
    number: u32,
    #[serde(rename = "dozorScore")]
    score: f64,
    #[serde(rename = "dozorSpotsNumOf")]
    spots: u64,
    #[serde(rename = "dozorVisibleResolution")]
    resolution: f64,
}

#[derive(Deserialize)]
struct RawFile {
    #[serde(rename = "imageQualityIndicators")]
    indicators: Vec<RawFrame>,
}

// ─── Public data types ────────────────────────────────────────────────────────

pub struct DozorFrame {
    pub number: u32,
    pub score: f32,
    pub spots: f32,
    pub resolution: f32,
}

pub struct DozorData {
    pub frames: Vec<DozorFrame>,
    // Per-series min/max for normalization.
    pub score_range: (f32, f32),
    pub spots_range: (f32, f32),
    pub resolution_range: (f32, f32),
}

fn value_range(iter: impl Iterator<Item = f32>) -> (f32, f32) {
    let (lo, hi) = iter.fold((f32::INFINITY, f32::NEG_INFINITY), |(lo, hi), v| {
        (lo.min(v), hi.max(v))
    });
    // Guard against a flat series.
    if (hi - lo).abs() < 1e-6 { (lo - 1.0, hi + 1.0) } else { (lo, hi) }
}

impl DozorData {
    fn build(mut raw: Vec<RawFrame>) -> Self {
        raw.sort_by_key(|r| r.number);
        let frames: Vec<DozorFrame> = raw.iter().map(|r| DozorFrame {
            number: r.number,
            score: r.score as f32,
            spots: r.spots as f32,
            resolution: r.resolution as f32,
        }).collect();
        let score_range      = value_range(frames.iter().map(|f| f.score));
        let spots_range      = value_range(frames.iter().map(|f| f.spots));
        let resolution_range = value_range(frames.iter().map(|f| f.resolution));
        Self { frames, score_range, spots_range, resolution_range }
    }
}

// ─── Path finding ─────────────────────────────────────────────────────────────

/// Given a raw master file, derive the path to the PyDozor JSON if it exists.
///
/// Expected raw layout:
///   `…/<date>/raw/<protein>/<sample>/<sample>_<run>_master.h5`
///
/// Corresponding process layout:
///   `…/<date>/process/<protein>/<sample>/xds_<sample>_<run>_*/ControlPyDozor_*/outDataControlPyDozor.json`
pub fn find_dozor_json(master_path: &Path) -> Option<PathBuf> {
    let filename   = master_path.file_name()?.to_str()?;
    let dataset    = filename.strip_suffix("_master.h5")?; // e.g. "AaIspE-F2X-Entry-H1b_3"

    let sample_dir = master_path.parent()?;
    let sample     = sample_dir.file_name()?.to_str()?;    // e.g. "AaIspE-F2X-Entry-H1b"

    let protein_dir = sample_dir.parent()?;
    let protein     = protein_dir.file_name()?.to_str()?;  // e.g. "AaIspE"

    let raw_dir = protein_dir.parent()?;
    if raw_dir.file_name()?.to_str()? != "raw" {
        return None;
    }
    let date_dir = raw_dir.parent()?;

    let proc_sample_dir = date_dir.join("process").join(protein).join(sample);
    find_dozor_in(&proc_sample_dir, dataset)
}

fn find_dozor_in(sample_dir: &Path, dataset: &str) -> Option<PathBuf> {
    let xds_prefix = format!("xds_{dataset}_");
    let mut xds_dirs: Vec<PathBuf> = std::fs::read_dir(sample_dir).ok()?
        .filter_map(|e| e.ok())
        .filter(|e| {
            let n = e.file_name().to_string_lossy().into_owned();
            n.starts_with(&xds_prefix) && e.file_type().map(|t| t.is_dir()).unwrap_or(false)
        })
        .map(|e| e.path())
        .collect();
    xds_dirs.sort();

    for xds_dir in &xds_dirs {
        let mut dozor_dirs: Vec<PathBuf> = std::fs::read_dir(xds_dir).ok()?.
            filter_map(|e| e.ok())
            .filter(|e| {
                let n = e.file_name().to_string_lossy().into_owned();
                n.starts_with("ControlPyDozor_") && e.file_type().map(|t| t.is_dir()).unwrap_or(false)
            })
            .map(|e| e.path())
            .collect();
        dozor_dirs.sort();

        for dozor_dir in &dozor_dirs {
            let json = dozor_dir.join("outDataControlPyDozor.json");
            if json.exists() {
                return Some(json);
            }
        }
    }
    None
}

// ─── Loading ──────────────────────────────────────────────────────────────────

pub fn load_dozor(path: &Path) -> Option<DozorData> {
    let text = std::fs::read_to_string(path).ok()?;
    let raw: RawFile = serde_json::from_str(&text).ok()?;
    if raw.indicators.is_empty() { return None; }
    Some(DozorData::build(raw.indicators))
}

// ─── Chart rendering ──────────────────────────────────────────────────────────

const SCORE_COLOR: egui::Color32 = egui::Color32::from_rgb(255, 200, 50);  // amber
const SPOTS_COLOR: egui::Color32 = egui::Color32::from_rgb(80,  200, 255); // cyan
const RESOL_COLOR: egui::Color32 = egui::Color32::from_rgb(100, 210, 100); // green

/// Draw the quality chart inside `rect` using the given `painter`.
/// `current` is the 0-based frame index currently displayed in the viewport.
pub fn draw_chart(painter: &egui::Painter, rect: egui::Rect, data: &DozorData, current: usize) {
    let n = data.frames.len();
    if n == 0 { return; }

    // Background.
    painter.rect_filled(rect, 0.0, egui::Color32::from_rgba_unmultiplied(0, 0, 0, 215));

    // Layout: legend row at top, then the plot area.
    let legend_h = 16.0;
    let pad      = 4.0;
    let plot = egui::Rect::from_min_max(
        egui::pos2(rect.left()  + pad, rect.top()    + legend_h + pad),
        egui::pos2(rect.right() - pad, rect.bottom() - pad),
    );
    if plot.width() < 4.0 || plot.height() < 4.0 { return; }

    let first_number = data.frames.first().map_or(0.0, |f| f.number as f32);
    let last_number = data.frames.last().map_or(first_number, |f| f.number as f32);
    let number_span = (last_number - first_number).max(1.0);
    let x_of = |number: u32| {
        let t = (number as f32 - first_number) / number_span;
        plot.left() + t.clamp(0.0, 1.0) * plot.width()
    };
    let norm = |v: f32, (lo, hi): (f32, f32)| ((v - lo) / (hi - lo)).clamp(0.0, 1.0);
    let y_of = |t: f32| plot.bottom() - t * plot.height();

    // Score.
    draw_polyline(painter, (0..n).map(|i| {
        egui::pos2(x_of(data.frames[i].number), y_of(norm(data.frames[i].score, data.score_range)))
    }), SCORE_COLOR);

    // Spots.
    draw_polyline(painter, (0..n).map(|i| {
        egui::pos2(x_of(data.frames[i].number), y_of(norm(data.frames[i].spots, data.spots_range)))
    }), SPOTS_COLOR);

    // Resolution — inverted: lower Å = better = higher on chart.
    draw_polyline(painter, (0..n).map(|i| {
        egui::pos2(x_of(data.frames[i].number), y_of(1.0 - norm(data.frames[i].resolution, data.resolution_range)))
    }), RESOL_COLOR);

    // Current-frame vertical marker.
    let ci = current.min(n - 1);
    let cx = x_of(data.frames[ci].number);
    painter.line_segment(
        [egui::pos2(cx, rect.top()), egui::pos2(cx, rect.bottom())],
        egui::Stroke::new(1.0, egui::Color32::from_rgba_unmultiplied(255, 255, 255, 130)),
    );

    // Legend: colored swatch + "Label value" for the current frame.
    let frame    = &data.frames[ci];
    let font     = egui::FontId::proportional(11.0);
    let ly       = rect.top() + 2.0;
    let items = [
        (format!("Score {:.1}",   frame.score),        SCORE_COLOR),
        (format!("Spots {}",      frame.spots as u32),  SPOTS_COLOR),
        (format!("Res {:.3} Å",   frame.resolution),    RESOL_COLOR),
    ];

    let mut lx = rect.left() + 6.0;
    for (text, color) in &items {
        // Small colored dash.
        painter.line_segment(
            [egui::pos2(lx, ly + 5.5), egui::pos2(lx + 10.0, ly + 5.5)],
            egui::Stroke::new(2.0, *color),
        );
        lx += 13.0;
        // Label text.
        let used = painter.text(
            egui::pos2(lx, ly),
            egui::Align2::LEFT_TOP,
            text,
            font.clone(),
            *color,
        );
        lx = used.right() + 12.0;
    }
}

fn draw_polyline(painter: &egui::Painter, pts: impl Iterator<Item = egui::Pos2>, color: egui::Color32) {
    let points: Vec<egui::Pos2> = pts.collect();
    if points.len() >= 2 {
        painter.add(egui::Shape::line(points, egui::Stroke::new(1.0, color)));
    }
}
