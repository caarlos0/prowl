//! Dashboard rendering: the styled, aligned view is painted **directly onto an
//! `uncurses` surface** — an offscreen [`TextBuffer`](uncurses::buffer::TextBuffer)
//! for one-shot output, or the watch [`Screen`](uncurses::screen::Screen). There is
//! one painter, so the layout lives in exactly one place.
//!
//! Width math uses the surface's own [`str_width`](uncurses::text::TextSurface::str_width)
//! and column gaps are implicit (unpainted cells stay blank, so no padding is
//! emitted). Each cell's OSC-8 link rides in its style, and the surface's color
//! [`Profile`](uncurses::color::Profile) downsamples styling at encode/present
//! time — so piped output (a `Disabled` profile) degrades to plain text with no
//! special-casing here.

use crate::status;
use uncurses::ansi::truncate::truncate as truncate_tail;
use uncurses::color::Color;
use uncurses::style::Style;
use uncurses::text::TextSurface;

/// The whole dashboard is kept within this many display columns; the flexible
/// title column is truncated (with an ellipsis) to make every table fit.
pub const MAX_WIDTH: usize = 120;

/// Two blank columns separate adjacent table columns.
const SEP: usize = 2;

/// One table cell: visible text plus its style. The style carries any OSC-8
/// link (uncurses styles hold the hyperlink), so there is no separate field.
#[derive(Clone, Debug)]
pub struct Cell {
    pub text: String,
    pub style: Style,
}

impl Cell {
    pub fn plain(text: impl Into<String>) -> Cell {
        Cell {
            text: text.into(),
            style: Style::new(),
        }
    }

    pub fn styled(text: impl Into<String>, style: impl Into<Style>) -> Cell {
        Cell {
            text: text.into(),
            style: style.into(),
        }
    }

    /// A dim + underlined OSC-8 hyperlink whose visible text is `text`.
    pub fn link(text: impl Into<String>, url: impl Into<String>) -> Cell {
        Cell {
            text: text.into(),
            style: Style::new().faint().underline().link(url.into(), ""),
        }
    }

    /// An OSC-8 hyperlink carrying an explicit style (e.g. a colored, clickable
    /// PR number). Underlined so it reads as a link even when colored.
    pub fn link_styled(
        text: impl Into<String>,
        url: impl Into<String>,
        style: impl Into<Style>,
    ) -> Cell {
        Cell {
            text: text.into(),
            style: style.into().underline().link(url.into(), ""),
        }
    }

    /// A styled, clickable `#<number>` PR link.
    pub fn pr(number: i64, url: impl Into<String>, style: impl Into<Style>) -> Cell {
        Cell::link_styled(format!("#{number}"), url, style)
    }
}

/// A table is a fixed header plus styled rows.
pub struct Table {
    pub header: Vec<&'static str>,
    pub rows: Vec<Vec<Cell>>,
}

/// Truncate `s` to at most `max` display columns, marking the cut with an
/// ellipsis (`\u{22ef}`, or `...` in ASCII mode). Delegates to the uncurses
/// width-aware truncator, so it counts display columns, not bytes.
pub fn truncate(s: &str, max: usize, ascii: bool) -> String {
    truncate_tail(s, max, if ascii { "..." } else { "\u{22ef}" })
}

/// The display width of table column `c`: its header and widest cell.
fn col_width(s: &impl TextSurface, table: &Table, c: usize) -> usize {
    let mut w = s.str_width(table.header[c]) as usize;
    for row in &table.rows {
        if let Some(cell) = row.get(c) {
            w = w.max(s.str_width(&cell.text) as usize);
        }
    }
    w
}

/// The width of every column of `table` except `skip`, plus the separators —
/// i.e. how wide a row is without its flexible column.
fn fixed_width(s: &impl TextSurface, table: &Table, skip: usize) -> usize {
    let cols = table.header.len();
    let total: usize = (0..cols)
        .filter(|&c| c != skip)
        .map(|c| col_width(s, table, c))
        .sum();
    total + SEP * cols.saturating_sub(1)
}

/// The shared `TITLE` column width across `tables`, capped so the widest row of
/// every table fits within [`MAX_WIDTH`]. Pass this to [`paint_table`] so the
/// section tables line up and the whole view stays within the budget.
pub fn title_width(s: &impl TextSurface, tables: &[&Table]) -> usize {
    let mut natural = 0;
    let mut fixed = 0;
    for t in tables {
        if let Some(ti) = t.header.iter().position(|h| *h == "TITLE") {
            natural = natural.max(col_width(s, t, ti));
            fixed = fixed.max(fixed_width(s, t, ti));
        }
    }
    natural.min(MAX_WIDTH.saturating_sub(fixed))
}

/// Paint `table` onto `s` starting at row `top`, forcing the `TITLE` column to
/// `title_w` columns when present (titles longer than that are ellipsized).
/// Columns are left-aligned and separated by two blank columns; the header row
/// is bold. Returns the next free row.
pub fn paint_table(
    s: &mut impl TextSurface,
    table: &Table,
    title_w: usize,
    ascii: bool,
    top: u16,
) -> u16 {
    let cols = table.header.len();
    let title_idx = table.header.iter().position(|h| *h == "TITLE");

    let mut widths: Vec<usize> = (0..cols).map(|c| col_width(s, table, c)).collect();
    if let Some(ti) = title_idx {
        widths[ti] = title_w;
    }

    // Column start positions: running sum of widths plus the separators.
    let mut xs = vec![0u16; cols];
    let mut acc = 0usize;
    for i in 0..cols {
        xs[i] = acc as u16;
        acc += widths[i] + SEP;
    }

    let bold = Style::new().bold();
    for (i, h) in table.header.iter().enumerate() {
        if !h.is_empty() {
            s.set_str((xs[i], top), h, &bold);
        }
    }

    for (r, row) in table.rows.iter().enumerate() {
        let y = top + 1 + r as u16;
        for (i, cell) in row.iter().enumerate() {
            let text = if Some(i) == title_idx {
                truncate(&cell.text, widths[i], ascii)
            } else {
                cell.text.clone()
            };
            s.set_str((xs[i], y), &text, &cell.style);
        }
    }
    top + 1 + table.rows.len() as u16
}

/// Paint a dim one-liner (an empty-section placeholder, the error line) at row
/// `y`. Returns y + 1.
pub fn paint_dim(s: &mut impl TextSurface, msg: &str, y: u16) -> u16 {
    s.set_str((0, y), msg, Style::new().faint());
    y + 1
}

/// Paint a section header at row `y`: a colored bold accent bar, the title, and
/// an optional dim count badge (or `Title (count)` in ASCII mode). Returns y + 1.
pub fn paint_header(
    s: &mut impl TextSurface,
    title: &str,
    accent: Color,
    count: Option<&str>,
    ascii: bool,
    y: u16,
) -> u16 {
    if ascii {
        let text = match count {
            Some(c) => format!("{title} ({c})"),
            None => title.to_string(),
        };
        s.set_str((0, y), &text, None);
    } else {
        let end = s.set_str(
            (0, y),
            &format!("\u{258c} {title}"),
            status::fg(accent).bold(),
        );
        if let Some(c) = count {
            s.set_str((end.x + 2, y), c, Style::new().faint());
        }
    }
    y + 1
}

/// A status glyph cell: the Nerd Font glyph (or ASCII letter) in the status's
/// palette color.
pub fn status_cell(status: status::Status, ascii: bool) -> Cell {
    Cell::styled(
        status::glyph(status, ascii).to_string(),
        status::fg(status::status_style(status).1),
    )
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

/// Paint the watch-mode key-hint footer at row `y`, folding the next-refresh
/// countdown into the refresh hint: `r refresh (next in 5m) - ? help`. Each key
/// glyph is a bold muted accent, its labels dim; plain in ASCII mode. Returns
/// y + 1.
pub fn paint_footer(s: &mut impl TextSurface, next: &str, ascii: bool, y: u16) -> u16 {
    if ascii {
        s.set_str(
            (0, y),
            &format!("r refresh (next in {next}) - ? help"),
            None,
        );
    } else {
        let key = status::fg(status::OVERLAY).bold();
        let dim = Style::new().faint();
        let p = s.set_str((0, y), "r", &key);
        let p = s.set_str((p.x + 1, y), &format!("refresh (next in {next})"), &dim);
        let p = s.set_str((p.x, y), " - ", &dim);
        let p = s.set_str((p.x, y), "?", &key);
        s.set_str((p.x + 1, y), "help", &dim);
    }
    y + 1
}

/// Paint one indented `glyph  meaning` legend row at `y`: the glyph in `gstyle`,
/// two blank columns, then the meaning in `dim`.
fn legend_row(
    s: &mut impl TextSurface,
    glyph: &str,
    gstyle: Style,
    meaning: &str,
    dim: &Style,
    y: u16,
) {
    let p = s.set_str((2, y), glyph, gstyle);
    s.set_str((p.x + 2, y), meaning, dim);
}

/// Paint the help legend at row `top`: a complete reference of every status
/// glyph and every `mergeStateStatus` value. Returns the next free row.
pub fn paint_help(s: &mut impl TextSurface, ascii: bool, top: u16) -> u16 {
    let dim = Style::new().faint();
    let mut y = paint_header(s, "Help", status::OVERLAY, None, ascii, top);

    for st in status::ORDER {
        let glyph = status::glyph(st, ascii).to_string();
        let color = status::fg(status::status_style(st).1);
        legend_row(s, &glyph, color, status::status_meaning(st), &dim, y);
        y += 1;
    }
    s.set_str((2, y), "- no checks reported yet", &dim);
    y += 1;

    for st in status::STATE_ORDER {
        let meaning = status::state_meaning(st);
        let c = status::state_style(st);
        if ascii {
            // Label form (matches the ASCII/piped STATE column).
            let p = s.set_str((2, y), status::state_label(st), c);
            if !meaning.is_empty() {
                s.set_str((p.x, y), &format!(" \u{2014} {meaning}"), &dim);
            }
        } else {
            // Glyph form (matches the Nerd Font STATE column).
            legend_row(s, &status::state_glyph(st).to_string(), c, meaning, &dim, y);
        }
        y += 1;
    }
    y
}

#[cfg(test)]
mod tests {
    use super::*;
    use uncurses::buffer::TextBuffer;
    use uncurses::color::Profile;
    use uncurses::text::Encode;

    /// Paint `f` into a fresh buffer and return its encoded form at `profile`.
    fn encode(
        width: u16,
        height: u16,
        profile: Profile,
        f: impl FnOnce(&mut TextBuffer),
    ) -> String {
        let mut canvas = TextBuffer::new(width, height);
        f(&mut canvas);
        let mut out = Vec::new();
        canvas.encode_with(&mut out, profile).unwrap();
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn plain_table_aligns_and_pads() {
        let table = Table {
            header: vec!["PR", "TITLE"],
            rows: vec![
                vec![Cell::plain("#1"), Cell::plain("short")],
                vec![Cell::plain("#42"), Cell::plain("x")],
            ],
        };
        let out = encode(20, 3, Profile::Disabled, |b| {
            paint_table(b, &table, 5, true, 0);
        });
        // Header, then two rows; columns line up by display width, no escapes.
        assert_eq!(out, "PR   TITLE\r\n#1   short\r\n#42  x");
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
        let out = encode(10, 3, Profile::Disabled, |b| {
            paint_table(b, &table, 0, true, 0);
        });
        let lines: Vec<&str> = out.split("\r\n").collect();
        let col = |line: &str| {
            line.find('#')
                .map(|i| &line[..i])
                .unwrap_or("")
                .chars()
                .count()
        };
        // The "#" starts at the same display column on both rows.
        assert_eq!(col(lines[1]), col(lines[2]));
    }

    #[test]
    fn styled_url_is_an_osc8_hyperlink() {
        let table = Table {
            header: vec!["URL"],
            rows: vec![vec![Cell::link("https://x/1", "https://x/1")]],
        };
        let out = encode(12, 2, Profile::TrueColor, |b| {
            paint_table(b, &table, 0, false, 0);
        });
        // OSC-8 framing around the link text; dim + underline SGR.
        assert!(out.contains("\x1b]8;;https://x/1\x1b\\"));
        assert!(out.contains("\x1b]8;;\x1b\\"));
        assert!(out.contains("\x1b[2;4m"));
    }

    #[test]
    fn disabled_profile_drops_styling_and_links() {
        let table = Table {
            header: vec!["URL"],
            rows: vec![vec![Cell::link("https://x/1", "https://x/1")]],
        };
        let out = encode(12, 2, Profile::Disabled, |b| {
            paint_table(b, &table, 0, false, 0);
        });
        assert!(!out.contains('\x1b'));
        assert!(out.contains("https://x/1"));
    }

    #[test]
    fn footer_is_plain_or_styled_key_hints() {
        let plain = encode(40, 1, Profile::Disabled, |b| {
            paint_footer(b, "5m", true, 0);
        });
        assert_eq!(plain, "r refresh (next in 5m) - ? help");

        let styled = encode(40, 1, Profile::TrueColor, |b| {
            paint_footer(b, "5m", false, 0);
        });
        assert!(styled.contains("refresh (next in 5m)"));
        assert!(styled.contains("help"));
        // Bold key accent (combined with the muted color) and a dim label.
        assert!(styled.contains("\x1b[1;"));
        assert!(styled.contains("\x1b[2m"));
    }

    #[test]
    fn truncate_marks_cut_with_ellipsis() {
        assert_eq!(truncate("short", 10, false), "short");
        assert_eq!(truncate("hello world", 8, false), "hello w\u{22ef}");
        assert_eq!(truncate("hello world", 8, true), "hello...");
    }

    #[test]
    fn title_column_is_capped_to_max_width() {
        let long = "x".repeat(200);
        let table = Table {
            header: vec!["", "PR", "TITLE", "BASE"],
            rows: vec![vec![
                Cell::plain(" "),
                Cell::plain("#1"),
                Cell::plain(long),
                Cell::plain("main"),
            ]],
        };
        let mut canvas = TextBuffer::new(MAX_WIDTH as u16, 2);
        let tw = title_width(&canvas, &[&table]);
        paint_table(&mut canvas, &table, tw, false, 0);
        let out = canvas.display_with(Profile::Disabled).to_string();
        for line in out.split("\r\n") {
            assert!(line.chars().count() <= MAX_WIDTH, "line exceeds MAX_WIDTH");
        }
        // The title was truncated with the ellipsis.
        assert!(out.contains('\u{22ef}'));
    }
}
