//! Saved targets: a small personal list of named remote-control targets
//! ("Dev till → 192.0.2.10"), managed from the start screen.
//!
//! Persisted next to `recent.txt` in the per-user data dir as `servers.txt`,
//! one `name<TAB>host` per line. The user never edits it; the start screen
//! adds, renames and removes entries. One entry per host (case-insensitive),
//! so renaming is simply saving the same host under a new name.

const FILE: &str = "servers.txt";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Server {
    pub name: String,
    pub host: String,
}

fn path() -> std::path::PathBuf {
    crate::paths::data_dir().join(FILE)
}

/// Saved targets, sorted by name.
pub fn load() -> Vec<Server> {
    parse(&std::fs::read_to_string(path()).unwrap_or_default())
}

/// Save `host` under `name` (replaces an existing entry for the host).
pub fn upsert(name: &str, host: &str) {
    let mut list = load();
    if upsert_in(&mut list, name, host) {
        store(&list);
    }
}

/// Remove the entry for `host`, if any.
pub fn remove(host: &str) {
    let mut list = load();
    let before = list.len();
    list.retain(|s| !s.host.eq_ignore_ascii_case(host.trim()));
    if list.len() != before {
        store(&list);
    }
}

/// The host saved under the name `text` (case-insensitive), if any.
pub fn resolve(text: &str) -> Option<String> {
    resolve_in(&load(), text)
}

/// `Name (host)` for a saved host, else the host itself. For titles and
/// status lines.
pub fn display(host: &str) -> String {
    match load()
        .into_iter()
        .find(|s| s.host.eq_ignore_ascii_case(host.trim()))
    {
        Some(s) => format!("{} ({})", s.name, s.host),
        None => host.to_string(),
    }
}

/// Atomic write (temp file + rename), like `recent::add`.
fn store(list: &[Server]) {
    let p = path();
    let tmp = p.with_extension("tmp");
    if std::fs::write(&tmp, serialize(list)).is_ok() {
        let _ = std::fs::rename(&tmp, &p);
    }
}

/// Names and hosts are single-line; tabs would break the format.
fn clean(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect::<String>()
        .trim()
        .to_string()
}

pub(crate) fn parse(text: &str) -> Vec<Server> {
    let mut list: Vec<Server> = Vec::new();
    for line in text.lines() {
        let Some((name, host)) = line.split_once('\t') else {
            continue;
        };
        let (name, host) = (clean(name), clean(host));
        if name.is_empty() || host.is_empty() {
            continue;
        }
        upsert_in(&mut list, &name, &host);
    }
    list
}

pub(crate) fn serialize(list: &[Server]) -> String {
    list.iter()
        .map(|s| format!("{}\t{}\n", s.name, s.host))
        .collect()
}

/// Insert or rename; keeps the list sorted by name. Returns false for an
/// empty name/host (nothing changed).
pub(crate) fn upsert_in(list: &mut Vec<Server>, name: &str, host: &str) -> bool {
    let (name, host) = (clean(name), clean(host));
    if name.is_empty() || host.is_empty() {
        return false;
    }
    list.retain(|s| !s.host.eq_ignore_ascii_case(&host));
    list.push(Server { name, host });
    list.sort_by_key(|s| s.name.to_lowercase());
    true
}

pub(crate) fn resolve_in(list: &[Server], text: &str) -> Option<String> {
    let text = text.trim();
    list.iter()
        .find(|s| s.name.eq_ignore_ascii_case(text))
        .map(|s| s.host.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn srv(name: &str, host: &str) -> Server {
        Server {
            name: name.into(),
            host: host.into(),
        }
    }

    #[test]
    fn round_trip_and_sorting() {
        let mut list = Vec::new();
        upsert_in(&mut list, "Zeta", "192.0.2.2");
        upsert_in(&mut list, "alpha", "192.0.2.1");
        assert_eq!(
            list,
            vec![srv("alpha", "192.0.2.1"), srv("Zeta", "192.0.2.2")]
        );
        assert_eq!(parse(&serialize(&list)), list);
    }

    #[test]
    fn malformed_lines_are_skipped() {
        let list = parse("no tab here\n\t192.0.2.1\nName\t\nOk\t192.0.2.3\r\n");
        assert_eq!(list, vec![srv("Ok", "192.0.2.3")]);
    }

    #[test]
    fn one_entry_per_host_and_rename() {
        let mut list = Vec::new();
        upsert_in(&mut list, "Till", "host-a");
        upsert_in(&mut list, "Till renamed", "HOST-A");
        assert_eq!(list, vec![srv("Till renamed", "HOST-A")]);
    }

    #[test]
    fn empty_or_control_chars_are_cleaned() {
        let mut list = Vec::new();
        assert!(!upsert_in(&mut list, "  ", "h"));
        assert!(upsert_in(&mut list, "A\tB", " h \n"));
        assert_eq!(list, vec![srv("A B", "h")]);
        assert_eq!(parse(&serialize(&list)), list);
    }

    #[test]
    fn resolve_by_name_case_insensitive() {
        let list = vec![srv("Dev Till", "192.0.2.10")];
        assert_eq!(resolve_in(&list, " dev till "), Some("192.0.2.10".into()));
        assert_eq!(resolve_in(&list, "192.0.2.10"), None);
    }
}
