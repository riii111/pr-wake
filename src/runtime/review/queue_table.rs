use std::fmt::Write as _;

use unicode_segmentation::UnicodeSegmentation as _;
use unicode_width::UnicodeWidthStr as _;

use super::{QueueEntry, one_line};

const CHAT: &str = "レビューチャット";

fn clipped(text: &str, limit: usize) -> String {
    if text.width() <= limit {
        return text.to_owned();
    }
    if limit < 3 {
        return ".".repeat(limit);
    }
    let mut out = String::new();
    let mut used = 0;
    for cluster in text.graphemes(true) {
        used += cluster.width();
        if used > limit - 3 {
            break;
        }
        out.push_str(cluster);
    }
    out.push_str("...");
    out
}

fn link(label: &str, url: &str, color: bool) -> String {
    if url.chars().any(char::is_control) {
        return label.to_owned();
    }
    let (start, end) = if color {
        ("\x1b[36m", "\x1b[39m")
    } else {
        ("", "")
    };
    format!("\x1b]8;;{url}\x1b\\{start}{label}{end}\x1b]8;;\x1b\\")
}

pub(super) fn plain_links(entries: &[QueueEntry]) -> String {
    let mut out = String::new();
    for entry in entries {
        let _ = writeln!(
            out,
            "{}#{} {}  {}",
            entry.repo,
            entry.number,
            one_line(&entry.title),
            entry.url
        );
        if let Some(id) = &entry.review_thread {
            let _ = writeln!(out, "  codex://threads/{id}");
        }
    }
    if entries.is_empty() {
        out.push_str("no queued requests\n");
    }
    out
}

pub(super) fn render(
    entries: &[QueueEntry],
    width: usize,
    color: bool,
    stripe: Option<&str>,
) -> String {
    let mut out = format!("レビュー待ち  {}件\n", entries.len());
    if entries.is_empty() {
        return out;
    }
    let prs: Vec<_> = entries
        .iter()
        .map(|e| {
            let repo = &e.repo;
            format!("{repo}#{}", e.number)
        })
        .collect();
    let pr_width = prs.iter().map(|pr| pr.width()).max().unwrap_or(2).max(2);
    let title_width = width.saturating_sub(pr_width + 4 + CHAT.width());
    let compact = title_width < 16;
    if !compact {
        let _ = writeln!(
            out,
            "\nPR{}  タイトル{}  {CHAT}",
            " ".repeat(pr_width - 2),
            " ".repeat(title_width - "タイトル".width())
        );
        let _ = writeln!(out, "{}", "-".repeat(width));
    }
    for (index, (entry, pr)) in entries.iter().zip(prs).enumerate() {
        let title = one_line(&entry.title).replace(char::is_control, "");
        let chat = entry.review_thread.as_ref().map_or_else(
            || "-".into(),
            |id| link("チャットを開く", &format!("codex://threads/{id}"), color),
        );
        let pr_link = link(&pr, &entry.url, color);
        if compact {
            let _ = writeln!(
                out,
                "\n{pr_link}  {chat}\n  {}",
                clipped(&title, width.saturating_sub(2))
            );
        } else {
            let title = clipped(&title, title_width);
            let background = stripe.filter(|_| color && index % 2 == 1).unwrap_or("");
            let reset = if background.is_empty() {
                ""
            } else {
                "\x1b[49m"
            };
            let _ = writeln!(
                out,
                "{background}{pr_link}{}  {title}{}  {chat}{reset}",
                " ".repeat(pr_width - pr.width()),
                " ".repeat(title_width - title.width())
            );
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chat_column_aligns_with_header_after_wide_and_combining_characters() {
        let mut entries =
            ["feat: 和文👩‍💻e\u{301}", "a".repeat(100).as_str()].map(|title| QueueEntry {
                key: String::new(),
                repo: "org/repo".into(),
                number: 123,
                title: title.into(),
                url: String::new(),
                requester: None,
                review_thread: None,
            });
        entries[0].url = "https://github.com/org/repo/pull/123".into();
        entries[0].review_thread = Some("test-thread".into());
        let plain = plain_links(&entries);
        assert!(plain.contains(&entries[0].url));
        assert!(plain.contains("codex://threads/test-thread"));
        let table = render(&entries, 80, false, None);
        let escape = regex::Regex::new(r"\x1b\]8;;[^\x1b]*\x1b\\").unwrap();
        let table = escape.replace_all(&table, "");
        let column = table
            .lines()
            .nth(2)
            .unwrap()
            .split_once(CHAT)
            .unwrap()
            .0
            .width();
        for line in table.lines().skip(4) {
            assert_eq!(
                line.strip_suffix('-')
                    .or_else(|| line.strip_suffix("チャットを開く"))
                    .unwrap()
                    .width(),
                column
            );
        }
    }
}
