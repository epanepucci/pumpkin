pub fn display_name(value: &str) -> Option<&str> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }

    Some(
        std::path::Path::new(value)
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or(value),
    )
}

pub fn elide_middle(value: &str, max_chars: usize) -> String {
    let len = value.chars().count();
    if len <= max_chars {
        return value.to_owned();
    }

    let keep = max_chars.saturating_sub(3);
    let front = keep / 2;
    let back = keep - front;
    let prefix: String = value.chars().take(front).collect();
    let suffix: String = value.chars().skip(len - back).collect();
    format!("{prefix}...{suffix}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_name_takes_last_path_component() {
        assert_eq!(display_name("  /data/run1/set_master.h5 "), Some("set_master.h5"));
        assert_eq!(display_name("   "), None);
    }

    #[test]
    fn elide_middle_keeps_ends_and_length() {
        assert_eq!(elide_middle("short", 10), "short");
        let out = elide_middle("abcdefghijklmnopqrstuvwxyz", 11);
        assert_eq!(out.chars().count(), 11);
        assert!(out.starts_with("abcd") && out.ends_with("wxyz") && out.contains("..."));
    }
}
