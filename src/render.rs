//! Terminal rendering: styled, aligned tables with OSC-8 hyperlinks, concise
//! section headers, the dim status line, and the bell. Every escape is gated on
//! a `styled` flag, so piped / non-TTY output is plain text.

use crate::status::{self, Rgb, Status};
use anstyle::Style;
use std::fmt::Write as _;
use std::io::Write as _;
use unicode_width::UnicodeWidthStr;

/// One table cell: visible text plus how to style and (optionally) link it.
#[derive(Clone, Debug)]
pub struct Cell {
    pub text: String,
    pub style: Style,
    pub link: Option<String>,
}

impl Cell {
    pub fn plain(text: impl Into<String>) -> Cell {
        Cell {
            text: text.into(),
            style: Style::new(),
            link: None,
        }
    }

    pub fn styled(text: impl Into<String>, style: Style) -> Cell {
        Cell {
            text: text.into(),
            style,
            link: None,
        }
    }

    /// A dim + underlined OSC-8 hyperlink whose visible text is `text`.
    pub fn link(text: impl Into<String>, url: impl Into<String>) -> Cell {
        Cell {
            text: text.into(),
            style: Style::new().dimmed().underline(),
            link: Some(url.into()),
        }
    }
}

/// A table is a fixed header plus styled rows.
pub struct Table {
    pub header: Vec<&'static str>,
    pub rows: Vec<Vec<Cell>>,
}

fn w(s: &str) -> usize {
    UnicodeWidthStr::width(s)
}

const OSC8_END: &str = "\x1b]8;;\x1b\\";

fn osc8_start(url: &str) -> String {
    format!("\x1b]8;;{url}\x1b\\")
}

/// Append one cell, padded to `col_w` unless it is the last column.
fn push_cell(out: &mut String, cell: &Cell, col_w: usize, last: bool, styled: bool) {
    if styled {
        if let Some(url) = &cell.link {
            out.push_str(&osc8_start(url));
        }
        let _ = write!(
            out,
            "{}{}{}",
            cell.style.render(),
            cell.text,
            cell.style.render_reset()
        );
        if cell.link.is_some() {
            out.push_str(OSC8_END);
        }
    } else {
        out.push_str(&cell.text);
    }
    if !last {
        for _ in 0..col_w.saturating_sub(w(&cell.text)) {
            out.push(' ');
        }
    }
}

/// Render a table. Columns are left-aligned, padded to the widest cell (by
/// display width), separated by two spaces; the header row is bold.
pub fn render_table(table: &Table, styled: bool) -> String {
    let cols = table.header.len();
    let mut widths = vec![0usize; cols];
    for (i, h) in table.header.iter().enumerate() {
        widths[i] = w(h);
    }
    for row in &table.rows {
        for (i, cell) in row.iter().enumerate() {
            if i < cols {
                widths[i] = widths[i].max(w(&cell.text));
            }
        }
    }

    let mut out = String::new();
    let header_style = if styled {
        Style::new().bold()
    } else {
        Style::new()
    };
    for (i, h) in table.header.iter().enumerate() {
        let last = i + 1 == cols;
        push_cell(
            &mut out,
            &Cell::styled((*h).to_string(), header_style),
            widths[i],
            last,
            styled,
        );
        if !last {
            out.push_str("  ");
        }
    }
    out.push('\n');
    for row in &table.rows {
        for (i, cell) in row.iter().enumerate() {
            let last = i + 1 == cols;
            push_cell(&mut out, cell, widths[i], last, styled);
            if !last {
                out.push_str("  ");
            }
        }
        out.push('\n');
    }
    out
}

/// A concise, non-figlet section header: a colored bold accent bar, the title,
/// and an optional dim row count.
pub fn header(title: &str, accent: Rgb, count: Option<usize>, styled: bool) -> String {
    if styled {
        let bar = status::fg(accent).bold();
        let dim = Style::new().dimmed();
        let count_part = match count {
            Some(c) => format!("  {}{}{}", dim.render(), c, dim.render_reset()),
            None => String::new(),
        };
        format!(
            "{}\u{258c} {title}{}{count_part}",
            bar.render(),
            bar.render_reset(),
        )
    } else {
        match count {
            Some(c) => format!("{title} ({c})"),
            None => title.to_string(),
        }
    }
}

/// A dim one-liner: an empty-section placeholder, or other plain dim status
/// text (the status line, the loading screen). Plain when not styled.
pub fn empty_line(msg: &str, styled: bool) -> String {
    if styled {
        let dim = Style::new().dimmed();
        format!("{}{msg}{}", dim.render(), dim.render_reset())
    } else {
        msg.to_string()
    }
}

/// The dim trailing status line: `updated HH:MM:SS — changed · next HH:MM:SS`.
pub fn status_line(hms: &str, change: Option<bool>, next: Option<&str>, styled: bool) -> String {
    let suffix = match change {
        Some(true) => " \u{2014} changed",
        Some(false) => " \u{2014} unchanged",
        None => "",
    };
    let next_part = match next {
        Some(n) => format!(" \u{00b7} next {n}"),
        None => String::new(),
    };
    let msg = format!("updated {hms}{suffix}{next_part}");
    empty_line(&msg, styled)
}

/// A leading cell marking a row that changed since the previous refresh.
pub fn change_marker(highlighted: bool, ascii: bool) -> Cell {
    if highlighted {
        let m = if ascii { ">" } else { "\u{25b8}" };
        Cell::styled(m, status::fg(status::PINK).bold())
    } else {
        Cell::plain(" ")
    }
}

/// A dim reference legend explaining the status glyphs and `STATE` values that
/// are currently on screen. `statuses` and `states` should already be
/// deduplicated; only entries that appear are listed.
pub fn reference(
    statuses: &[Status],
    has_none: bool,
    states: &[String],
    ascii: bool,
    styled: bool,
) -> String {
    let dim = Style::new().dimmed();
    let mut out = String::new();

    if styled {
        let h = dim.bold();
        let _ = writeln!(out, "{}Reference{}", h.render(), h.render_reset());
    } else {
        out.push_str("Reference\n");
    }

    for s in status::ORDER {
        if !statuses.contains(&s) {
            continue;
        }
        let ch = status::glyph(s, ascii);
        let meaning = status::status_meaning(s);
        if styled {
            let g = status::fg(status::status_style(s).1);
            let _ = writeln!(
                out,
                "  {}{ch}{}  {}{meaning}{}",
                g.render(),
                g.render_reset(),
                dim.render(),
                dim.render_reset()
            );
        } else {
            let _ = writeln!(out, "  {ch}  {meaning}");
        }
    }
    if has_none {
        let _ = writeln!(out, "  {}", empty_line("- no checks reported", styled));
    }

    // States in legend order, then any unknown extras.
    let mut ordered: Vec<&str> = status::STATE_ORDER
        .iter()
        .copied()
        .filter(|k| states.iter().any(|s| s == k))
        .collect();
    for s in states {
        if !ordered.contains(&s.as_str()) {
            ordered.push(s);
        }
    }
    for st in ordered {
        let meaning = status::state_meaning(st);
        let c = status::state_style(st);
        if ascii {
            // Label form (matches the ASCII/piped STATE column).
            let label = status::state_label(st);
            let tail = if meaning.is_empty() {
                String::new()
            } else {
                format!(" \u{2014} {meaning}")
            };
            if styled {
                let _ = writeln!(
                    out,
                    "  {}{label}{}{}{tail}{}",
                    c.render(),
                    c.render_reset(),
                    dim.render(),
                    dim.render_reset()
                );
            } else {
                let _ = writeln!(out, "  {label}{tail}");
            }
        } else {
            // Glyph form (matches the Nerd Font STATE column); always styled.
            let g = status::state_glyph(st);
            let _ = writeln!(
                out,
                "  {}{g}{}  {}{meaning}{}",
                c.render(),
                c.render_reset(),
                dim.render(),
                dim.render_reset()
            );
        }
    }
    out
}

/// Clear the screen and home the cursor.
pub fn clear() -> &'static str {
    "\x1b[2J\x1b[H"
}

/// Hide / show the terminal cursor.
pub const HIDE_CURSOR: &str = "\x1b[?25l";
pub const SHOW_CURSOR: &str = "\x1b[?25h";

/// The dim placeholder shown during the very first fetch, before any data has
/// been rendered.
pub fn loading(styled: bool) -> String {
    empty_line("Loading...", styled)
}

/// Ring the terminal bell once.
pub fn ring_bell() {
    print!("\x07");
    let _ = std::io::stdout().flush();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_table_aligns_and_pads() {
        let table = Table {
            header: vec!["PR", "TITLE"],
            rows: vec![
                vec![Cell::plain("#1"), Cell::plain("short")],
                vec![Cell::plain("#42"), Cell::plain("x")],
            ],
        };
        let out = render_table(&table, false);
        assert_eq!(
            out,
            // "PR " padded to width 3, two-space gap, last column unpadded.
            "PR   TITLE\n#1   short\n#42  x\n"
        );
    }

    #[test]
    fn padding_uses_display_width_for_glyphs() {
        // The check-circle glyph is one display column but several bytes; the
        // following column must still line up by display width.
        let glyph = status::status_style(status::Status::Pass).0;
        let table = Table {
            header: vec!["ST", "PR"],
            rows: vec![
                vec![Cell::plain(glyph.to_string()), Cell::plain("#1")],
                vec![Cell::plain("xx"), Cell::plain("#2")],
            ],
        };
        let out = render_table(&table, false);
        let lines: Vec<&str> = out.lines().collect();
        // Display width of everything before the PR column must match across
        // rows, even though the byte offsets differ (multi-byte glyph).
        let display_col = |line: &str| {
            let idx = line.find('#').unwrap();
            UnicodeWidthStr::width(&line[..idx])
        };
        assert_eq!(display_col(lines[1]), display_col(lines[2]));
    }

    #[test]
    fn styled_url_is_an_osc8_hyperlink() {
        let table = Table {
            header: vec!["URL"],
            rows: vec![vec![Cell::link("https://x/1", "https://x/1")]],
        };
        let out = render_table(&table, true);
        assert!(out.contains("\x1b]8;;https://x/1\x1b\\"));
        assert!(out.contains(OSC8_END));
        // dim + underline.
        assert!(out.contains("\x1b[2m"));
        assert!(out.contains("\x1b[4m"));
    }

    #[test]
    fn unstyled_url_is_just_text() {
        let table = Table {
            header: vec!["URL"],
            rows: vec![vec![Cell::link("https://x/1", "https://x/1")]],
        };
        let out = render_table(&table, false);
        assert!(!out.contains('\x1b'));
        assert!(out.contains("https://x/1"));
    }

    #[test]
    fn loading_is_plain_or_dim() {
        assert_eq!(loading(false), "Loading...");
        let styled = loading(true);
        assert!(styled.contains("Loading..."));
        assert!(styled.contains("\x1b[2m"));
    }

    #[test]
    fn status_line_suffix_and_next() {
        assert_eq!(
            status_line("12:00:00", None, None, false),
            "updated 12:00:00"
        );
        assert_eq!(
            status_line("12:00:00", Some(true), None, false),
            "updated 12:00:00 \u{2014} changed"
        );
        assert_eq!(
            status_line("12:00:00", Some(false), Some("12:10:00"), false),
            "updated 12:00:00 \u{2014} unchanged \u{00b7} next 12:10:00"
        );
    }
}
