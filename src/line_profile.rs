use crate::frame::Frame;

pub struct LineProfilePeak {
    pub index: usize,
    pub d_spacing: Option<f64>,
}

/// Peaks of `data` (a profile sampled from `start` to `end`) with the d-spacing at each.
pub fn peaks_with_resolution(
    frame: &Frame,
    start: egui::Pos2,
    end: egui::Pos2,
    data: &[f32],
) -> Vec<LineProfilePeak> {
    let indices = detect_peaks(data);
    if indices.is_empty() {
        return Vec::new();
    }

    let dx = end.x - start.x;
    let dy = end.y - start.y;
    let len = (dx * dx + dy * dy).sqrt();
    if len < 1.0 {
        return Vec::new();
    }
    let ux = dx / len;
    let uy = dy / len;

    indices
        .into_iter()
        .map(|index| {
            let x = start.x + ux * index as f32;
            let y = start.y + uy * index as f32;
            LineProfilePeak {
                index,
                d_spacing: crate::viewport::pixel_to_resolution(x as f64, y as f64, frame),
            }
        })
        .collect()
}

/// Sum `width` pixels perpendicular to the line from `start` to `end`, one value per step along it.
pub fn sample(frame: &Frame, start: egui::Pos2, end: egui::Pos2, width: u32) -> Vec<f32> {
    let dx = (end.x - start.x) as f64;
    let dy = (end.y - start.y) as f64;
    let len = (dx * dx + dy * dy).sqrt();
    if len < 1.0 {
        return vec![];
    }
    let steps = len as usize + 1;
    let tx = dx / len;
    let ty = dy / len;
    // Perpendicular direction for orthogonal sampling.
    let nx = -ty;
    let ny = tx;
    let half = width as i64 / 2;

    let mut profile = vec![0.0f32; steps];
    for (step, val) in profile.iter_mut().enumerate() {
        let cx = start.x as f64 + tx * step as f64;
        let cy = start.y as f64 + ty * step as f64;
        let mut sum = 0.0f32;
        for w in 0..width as i64 {
            let offset = w - half;
            let qx = (cx + offset as f64 * nx).round() as i64;
            let qy = (cy + offset as f64 * ny).round() as i64;
            if qx >= 0 && qy >= 0 && qx < frame.width as i64 && qy < frame.height as i64 {
                let pixel_index = (qy as u32 * frame.width + qx as u32) as usize;
                if !frame.is_masked_index(pixel_index) {
                    sum += frame.pixels[pixel_index] as f32;
                }
            }
        }
        *val = sum;
    }
    profile
}

pub fn detect_peaks(data: &[f32]) -> Vec<usize> {
    const MAX_PEAKS: usize = 12;
    if data.len() < 3 {
        return Vec::new();
    }

    let min_v = data.iter().copied().fold(f32::INFINITY, f32::min);
    let max_v = data.iter().copied().fold(f32::NEG_INFINITY, f32::max);
    let range = max_v - min_v;
    if !range.is_finite() || range <= 0.0 {
        return Vec::new();
    }

    let min_prominence = range * 0.08;
    let threshold = min_v + range * 0.15;
    let min_distance = (data.len() / 100).clamp(3, 20);
    let mut candidates = Vec::<(usize, f32)>::new();

    for i in 1..data.len() - 1 {
        let value = data[i];
        if value < threshold || value <= data[i - 1] || value < data[i + 1] {
            continue;
        }
        let left = data[i.saturating_sub(min_distance)..i]
            .iter()
            .copied()
            .fold(value, f32::min);
        let right = data[i + 1..=(i + min_distance).min(data.len() - 1)]
            .iter()
            .copied()
            .fold(value, f32::min);
        let prominence = value - left.max(right);
        if prominence >= min_prominence {
            candidates.push((i, value));
        }
    }

    candidates.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    let mut selected = Vec::<usize>::new();
    for (idx, _) in candidates {
        if selected
            .iter()
            .all(|&existing| existing.abs_diff(idx) >= min_distance)
        {
            selected.push(idx);
            if selected.len() == MAX_PEAKS {
                break;
            }
        }
    }
    selected.sort_unstable();
    selected
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_spaced_line_profile_peaks() {
        let mut data = vec![10.0; 80];
        data[18] = 100.0;
        data[42] = 140.0;
        data[65] = 120.0;

        assert_eq!(detect_peaks(&data), vec![18, 42, 65]);
    }

    #[test]
    fn ignores_flat_line_profiles() {
        assert!(detect_peaks(&[5.0; 32]).is_empty());
    }
}
