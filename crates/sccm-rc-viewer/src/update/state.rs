//! Persisted update settings: `update.txt` in the data directory, one
//! `key=value` per line. Unknown keys are kept as they are, so an older
//! viewer does not drop what a newer one wrote.

use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct State {
    /// Check for updates at start and every few hours.
    pub auto_check: bool,
    /// Build the user chose to skip ("Skip this version"); offered again
    /// only when a newer build appears.
    pub skipped_build: Option<u64>,
    other: Vec<(String, String)>,
}

impl Default for State {
    fn default() -> Self {
        Self {
            auto_check: true,
            skipped_build: None,
            other: Vec::new(),
        }
    }
}

impl State {
    pub fn parse(text: &str) -> Self {
        let mut s = Self::default();
        for line in text.lines() {
            let Some((k, v)) = line.split_once('=') else {
                continue;
            };
            let (k, v) = (k.trim(), v.trim());
            match k {
                "auto_check" => s.auto_check = v != "0",
                "skipped_build" => s.skipped_build = v.parse().ok(),
                _ if !k.is_empty() => s.other.push((k.to_string(), v.to_string())),
                _ => {}
            }
        }
        s
    }

    pub fn render(&self) -> String {
        let mut out = format!("auto_check={}\n", u8::from(self.auto_check));
        if let Some(b) = self.skipped_build {
            out.push_str(&format!("skipped_build={b}\n"));
        }
        for (k, v) in &self.other {
            out.push_str(&format!("{k}={v}\n"));
        }
        out
    }

    fn path() -> PathBuf {
        crate::paths::data_dir().join("update.txt")
    }

    pub fn load() -> Self {
        Self::load_from(&Self::path())
    }

    pub fn save(&self) {
        self.save_to(&Self::path());
    }

    fn load_from(path: &Path) -> Self {
        std::fs::read_to_string(path)
            .map(|t| Self::parse(&t))
            .unwrap_or_default()
    }

    /// Atomic write (temp file renamed over the target), as in `recent.rs`.
    fn save_to(&self, path: &Path) {
        let tmp = path.with_extension("tmp");
        if std::fs::write(&tmp, self.render()).is_ok() {
            let _ = std::fs::rename(&tmp, path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_to_auto_check_on() {
        assert_eq!(State::parse(""), State::default());
        assert!(State::default().auto_check);
    }

    #[test]
    fn round_trips_and_keeps_unknown_keys() {
        let s = State::parse("auto_check=0\nskipped_build=42\nfuture=x\n\ngarbage\n");
        assert!(!s.auto_check);
        assert_eq!(s.skipped_build, Some(42));
        assert_eq!(State::parse(&s.render()), s);
        assert!(s.render().contains("future=x"));
    }

    #[test]
    fn bad_build_number_is_ignored() {
        assert_eq!(State::parse("skipped_build=abc").skipped_build, None);
    }

    #[test]
    fn save_and_load_file() {
        let dir = std::env::temp_dir().join(format!("sccm-rc-upd-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("update.txt");
        let s = State {
            auto_check: false,
            skipped_build: Some(7),
            ..State::default()
        };
        s.save_to(&p);
        assert_eq!(State::load_from(&p), s);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
