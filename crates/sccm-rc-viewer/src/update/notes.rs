//! Release notes for the update sheet: the small Markdown subset the release
//! pipeline produces (the "## Release notes" sections of the merged pull
//! requests, see `.github/scripts/collect-release-notes.sh`), turned into
//! lines the sheet can style. Deliberately not a Markdown parser: headings,
//! bullets and plain text; emphasis, code marks and links become text.

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Line {
    Heading(String),
    Bullet(String),
    Text(String),
    /// Paragraph break (one, however many blank lines the source has).
    Blank,
}

pub fn render(md: &str) -> Vec<Line> {
    let mut out: Vec<Line> = Vec::new();
    for raw in md.lines() {
        let line = raw.trim_end();
        let t = line.trim_start();
        let item = if t.is_empty() {
            Line::Blank
        } else if let Some(h) = heading(t) {
            Line::Heading(inline(h))
        } else if let Some(b) = ["- ", "* ", "+ "].iter().find_map(|p| t.strip_prefix(p)) {
            Line::Bullet(inline(b.trim()))
        } else {
            Line::Text(inline(t))
        };
        match (&item, out.last()) {
            (Line::Blank, None | Some(Line::Blank)) => {}
            _ => out.push(item),
        }
    }
    while out.last() == Some(&Line::Blank) {
        out.pop();
    }
    out
}

fn heading(t: &str) -> Option<&str> {
    let rest = t.trim_start_matches('#');
    let level = t.len() - rest.len();
    ((1..=6).contains(&level) && rest.starts_with(' ')).then(|| rest.trim())
}

/// Inline marks to plain text: `**x**`, `__x__`, `` `x` `` → x, `[t](u)` → t.
fn inline(s: &str) -> String {
    let s = s.replace("**", "").replace("__", "").replace('`', "");
    let mut out = String::with_capacity(s.len());
    let mut rest = s.as_str();
    while let Some(open) = rest.find('[') {
        let after = &rest[open + 1..];
        let link = after.find("](").and_then(|close| {
            let url = &after[close + 2..];
            url.find(')').map(|end| (close, close + 2 + end + 1))
        });
        match link {
            Some((close, skip)) => {
                out.push_str(&rest[..open]);
                out.push_str(&after[..close]);
                rest = &after[skip..];
            }
            None => {
                out.push_str(&rest[..=open]);
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_the_pipeline_subset() {
        let md = "## What's new\n\n\n- Release notes are shown **in the sheet**.\n* See [the docs](https://x.example/a) and `--help`.\nThanks!\n\n";
        assert_eq!(
            render(md),
            vec![
                Line::Heading("What's new".into()),
                Line::Blank,
                Line::Bullet("Release notes are shown in the sheet.".into()),
                Line::Bullet("See the docs and --help.".into()),
                Line::Text("Thanks!".into()),
            ]
        );
    }

    #[test]
    fn leaves_odd_text_alone() {
        assert_eq!(render("#hashtag"), vec![Line::Text("#hashtag".into())]);
        assert_eq!(render("[a] b ["), vec![Line::Text("[a] b [".into())]);
        assert_eq!(render("-dash"), vec![Line::Text("-dash".into())]);
        assert!(render("\n\n").is_empty());
    }
}
