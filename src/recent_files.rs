use std::path::PathBuf;

/// Default number of recent files kept in the list (and on disk).
pub const DEFAULT_MAX_ENTRIES: usize = 20;

/// A dataset master file that was seen while monitoring the detector.
#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, PartialEq)]
pub struct MonitoredFile {
    /// Path of the `*_master.h5` file.
    pub path: String,
    /// When the series was last seen, seconds since the Unix epoch.
    pub seen_unix: u64,
}

#[derive(serde::Serialize, serde::Deserialize, Default)]
struct Stored {
    entries: Vec<MonitoredFile>,
}

/// Most-recently-first list of monitored master files, persisted in the app's
/// config folder so it survives restarts.
pub struct RecentMonitored {
    entries: Vec<MonitoredFile>,
    path: Option<PathBuf>,
    /// At most this many entries are kept, in memory and on disk.
    max_entries: usize,
}

impl RecentMonitored {
    /// Load from `~/.config/pumpkin/monitored_files.json`, keeping at most
    /// `max_entries` (newest first). A missing or corrupt file yields an empty
    /// list. A longer list on disk is trimmed the next time it is saved.
    pub fn load(max_entries: usize) -> Self {
        let path = crate::config::config_dir().map(|d| d.join("monitored_files.json"));
        let entries = path
            .as_ref()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .map(|text| Self::parse(&text))
            .unwrap_or_default();
        let mut list = Self { entries, path, max_entries };
        list.entries.truncate(max_entries);
        list
    }

    pub fn max_entries(&self) -> usize {
        self.max_entries
    }

    /// Change the limit, dropping the oldest entries beyond it and saving if any were dropped.
    pub fn set_max_entries(&mut self, max_entries: usize) {
        self.max_entries = max_entries;
        if self.entries.len() > max_entries {
            self.entries.truncate(max_entries);
            self.save();
        }
    }

    pub fn entries(&self) -> &[MonitoredFile] {
        &self.entries
    }

    /// Record that the series `series_id` written with `name_pattern` was
    /// monitored. Repeats move to the front.
    pub fn record(&mut self, name_pattern: &str, series_id: u64, now_unix: u64) {
        let path = master_path_from_pattern(name_pattern, series_id);
        if let Some(pos) = self.entries.iter().position(|e| e.path == path) {
            self.entries.remove(pos);
        }
        self.entries.insert(0, MonitoredFile { path, seen_unix: now_unix });
        self.entries.truncate(self.max_entries);
        self.save();
    }

    fn save(&self) {
        let Some(path) = &self.path else { return };
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        // Write-then-rename so a crash can't leave a truncated list behind.
        let tmp = path.with_extension("json.tmp");
        if std::fs::write(&tmp, self.to_json()).is_ok() {
            let _ = std::fs::rename(&tmp, path);
        }
    }

    fn to_json(&self) -> String {
        serde_json::to_string_pretty(&Stored { entries: self.entries.clone() }).unwrap_or_default()
    }

    fn parse(text: &str) -> Vec<MonitoredFile> {
        serde_json::from_str::<Stored>(text).map(|s| s.entries).unwrap_or_default()
    }
}

/// Master file for a filewriter `name_pattern` and series. `$id` is replaced by
/// the series id; the DECTRIS convention `<name>_master.h5` is added unless the
/// pattern already names the file.
pub fn master_path_from_pattern(pattern: &str, series_id: u64) -> String {
    let p = pattern.trim().replace("$id", &series_id.to_string());
    if p.ends_with(".h5") {
        p
    } else if p.ends_with("_master") {
        format!("{p}.h5")
    } else {
        format!("{p}_master.h5")
    }
}

/// "just now", "5 min ago", "3 h ago", "2 d ago".
pub fn format_age(seconds: u64) -> String {
    match seconds {
        0..=59 => "just now".to_string(),
        60..=3599 => format!("{} min ago", seconds / 60),
        3600..=86_399 => format!("{} h ago", seconds / 3600),
        _ => format!("{} d ago", seconds / 86_400),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn list() -> RecentMonitored {
        RecentMonitored { entries: Vec::new(), path: None, max_entries: DEFAULT_MAX_ENTRIES }
    }

    #[test]
    fn pattern_is_expanded_to_a_master_file() {
        assert_eq!(master_path_from_pattern("/d/lyso_$id", 7), "/d/lyso_7_master.h5");
        assert_eq!(master_path_from_pattern("/d/lyso", 7), "/d/lyso_master.h5");
        assert_eq!(master_path_from_pattern("/d/lyso_master", 7), "/d/lyso_master.h5");
        assert_eq!(master_path_from_pattern("/d/lyso_master.h5", 7), "/d/lyso_master.h5");
    }

    #[test]
    fn newest_first_deduplicated_and_capped() {
        let mut l = list();
        l.record("/d/a", 1, 100);
        l.record("/d/b", 1, 200);
        l.record("/d/a", 1, 300); // seen again: moves to the front, no duplicate
        let paths: Vec<_> = l.entries().iter().map(|e| e.path.as_str()).collect();
        assert_eq!(paths, ["/d/a_master.h5", "/d/b_master.h5"]);
        assert_eq!(l.entries()[0].seen_unix, 300);

        for i in 0..DEFAULT_MAX_ENTRIES + 5 {
            l.record(&format!("/d/x{i}"), 1, 400 + i as u64);
        }
        assert_eq!(l.entries().len(), DEFAULT_MAX_ENTRIES);
        assert!(l.entries()[0].path.contains(&format!("x{}", DEFAULT_MAX_ENTRIES + 4)));
    }

    #[test]
    fn lowering_the_limit_drops_the_oldest() {
        let mut l = list();
        for i in 0..5 {
            l.record(&format!("/d/x{i}"), 1, i as u64);
        }
        l.set_max_entries(2);
        let paths: Vec<_> = l.entries().iter().map(|e| e.path.as_str()).collect();
        assert_eq!(paths, ["/d/x4_master.h5", "/d/x3_master.h5"]);
        l.record("/d/new", 1, 10); // the new limit also applies to later records
        assert_eq!(l.entries().len(), 2);
        l.set_max_entries(10); // raising it doesn't bring anything back
        assert_eq!(l.entries().len(), 2);
        l.set_max_entries(0);
        assert!(l.entries().is_empty());
    }

    #[test]
    fn json_round_trip_and_corrupt_input() {
        let mut l = list();
        l.record("/d/a", 1, 100);
        l.record("/d/b", 2, 200);
        assert_eq!(RecentMonitored::parse(&l.to_json()), l.entries());
        assert!(RecentMonitored::parse("not json").is_empty());
    }

    #[test]
    fn age_formatting() {
        assert_eq!(format_age(5), "just now");
        assert_eq!(format_age(120), "2 min ago");
        assert_eq!(format_age(7200), "2 h ago");
        assert_eq!(format_age(3 * 86_400), "3 d ago");
    }
}
