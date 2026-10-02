//! Dashboard rendering: the styled, aligned view is painted **directly onto an
//! `uncurses` surface** — an offscreen [`TextBuffer`](uncurses::buffer::TextBuffer)
//! for one-shot output, or the watch [`Screen`](uncurses::screen::Screen). There is
//! one painter, so the layout lives in exactly one place.
//!
//! Width math uses the surface's own [`str_width`](uncurses::text::TextSurface::str_width)
//! and column gaps are implicit (unpainted cells stay blank, so no padding is
//! emitted). Each cell's OSC-8 link rides in its style, and the surface's color
//! [`Profile`](uncurses::color::Profile) downsamples styling at encode/render
//! time — so piped output (a `Disabled` profile) degrades to plain text with no
//! special-casing here.

use crate::cli::View;
use crate::status::{self, Lamp};
use std::borrow::Cow;
use uncurses::ansi::truncate::truncate as truncate_tail;
use uncurses::buffer::{Bounded, Surface, SurfaceMut, TextBuffer, View as BufView};
use uncurses::color::{Color, Profile};
use uncurses::layout::{Position, Rect};
use uncurses::style::Style;
use uncurses::text::{Encode, TextSurface};

/// Width used for piped output and screenshot rendering, where there is no live
/// terminal surface to size against.
pub const OUTPUT_WIDTH: usize = 120;

/// Below this width there is not enough room for a useful PR row after every
/// optional column has been removed.
pub const MIN_WIDTH: u16 = 24;

/// Two blank columns separate adjacent table columns.
const SEP: usize = 2;
const TITLE_MIN: usize = 5;
const BRANCH_MIN: usize = 6;

/// One lamp of the check semaphore: dim when zero, its palette color (bold)
/// when not, so only the counts that matter catch the eye.
pub fn lamp_cell(n: u64, lamp: Lamp) -> Cell {
    if n == 0 {
        Cell::styled("0".to_string(), Style::new().faint())
    } else {
        Cell::styled(n.to_string(), status::fg(status::lamp_color(lamp)).bold())
    }
}

/// One table cell: visible text plus its style. The style carries any OSC-8
/// link (uncurses styles hold the hyperlink), so there is no separate field.
#[derive(Clone, Debug)]
pub struct Cell {
    pub text: String,
    pub style: Style,
}

/// A per-URL OSC-8 `id=` parameter: the first 7 hex chars of the URL's hash, so
/// terminals treat each link (e.g. a PR number) as one distinct hyperlink and
/// don't merge adjacent ones.
fn link_params(url: &str) -> String {
    use std::hash::{DefaultHasher, Hash, Hasher};
    let mut h = DefaultHasher::new();
    url.hash(&mut h);
    let hex = format!("{:016x}", h.finish());
    format!("id={}", &hex[..7])
}

impl Cell {
    pub fn plain(text: impl Into<String>) -> Self {
        Self::styled(text, Style::new())
    }

    pub fn styled(text: impl Into<String>, style: impl Into<Style>) -> Self {
        Self {
            text: text.into(),
            style: style.into(),
        }
    }

    /// A dim + underlined OSC-8 hyperlink whose visible text is `text`.
    pub fn link(text: impl Into<String>, url: impl Into<String>) -> Self {
        Self::link_styled(text, url, Style::new().faint())
    }

    /// An OSC-8 hyperlink carrying an explicit style (e.g. a colored, clickable
    /// PR number). Underlined so it reads as a link even when colored.
    pub fn link_styled(
        text: impl Into<String>,
        url: impl Into<String>,
        style: impl Into<Style>,
    ) -> Self {
        let url = url.into();
        let params = link_params(&url);
        Self {
            text: text.into(),
            style: style.into().underline().link(url, params),
        }
    }

    /// A styled, clickable `#<number>` PR link.
    pub fn pr(number: i64, url: impl Into<String>, style: impl Into<Style>) -> Self {
        Self::link_styled(format!("#{number}"), url, style)
    }
}

/// A table is a fixed header plus styled rows.
pub struct Table {
    pub header: Vec<&'static str>,
    pub rows: Vec<Vec<Cell>>,
}

impl Table {
    fn column(&self, name: &str) -> Option<usize> {
        self.header.iter().position(|header| *header == name)
    }
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

struct TableLayout {
    columns: Vec<usize>,
    widths: Vec<usize>,
}

pub struct TableAlignment {
    prefix_widths: Vec<usize>,
    title_width: Option<usize>,
    hide_branch: bool,
}

fn aligned_col_width(
    s: &impl TextSurface,
    table: &Table,
    column: usize,
    title: usize,
    alignment: Option<&TableAlignment>,
) -> usize {
    if column < title
        && let Some(width) = alignment.and_then(|a| a.prefix_widths.get(column))
    {
        return *width;
    }
    col_width(s, table, column)
}

/// Lay a table out across the full surface width. Columns to the right of TITLE
/// disappear from right to left until the table fits. TITLE is the largest
/// flexible column and BRANCH is second.
fn table_layout(
    s: &impl TextSurface,
    table: &Table,
    alignment: Option<&TableAlignment>,
) -> Option<TableLayout> {
    let available = usize::from(s.bounds().width);
    let title = table.column("TITLE");
    let Some(title) = title else {
        let columns: Vec<usize> = (0..table.header.len()).collect();
        let widths = columns.iter().map(|&c| col_width(s, table, c)).collect();
        return Some(TableLayout { columns, widths });
    };
    let branch = table.column("BRANCH");
    let mut shown = vec![true; table.header.len()];
    if alignment.is_some_and(|a| a.hide_branch)
        && let Some(branch) = branch
    {
        shown[branch] = false;
    }

    loop {
        let columns: Vec<usize> = (0..table.header.len()).filter(|&c| shown[c]).collect();
        let separators = SEP * columns.len().saturating_sub(1);
        let fixed: usize = columns
            .iter()
            .filter(|&&c| c != title && Some(c) != branch)
            .map(|&c| aligned_col_width(s, table, c, title, alignment))
            .sum();
        let shown_branch = branch.filter(|&c| shown[c]);
        let branch_min = shown_branch.map_or(0, |_| BRANCH_MIN);
        let title_min = if shown_branch.is_some() {
            TITLE_MIN.max(BRANCH_MIN + 1)
        } else {
            TITLE_MIN
        };
        if fixed + separators + title_min + branch_min <= available {
            let flexible = available - fixed - separators;
            let (title_width, branch_width) = if branch_min == 0 {
                (flexible, None)
            } else if let Some(title_width) = alignment.and_then(|a| a.title_width) {
                (title_width, Some(flexible - title_width))
            } else {
                let natural = col_width(s, table, shown_branch.expect("shown branch has a column"));
                let branch_width = natural.clamp(BRANCH_MIN, ((flexible - 1) / 2).max(BRANCH_MIN));
                (flexible - branch_width, Some(branch_width))
            };
            let widths = columns
                .iter()
                .map(|&c| {
                    if c == title {
                        title_width
                    } else if Some(c) == branch {
                        branch_width.expect("shown branch has a width")
                    } else {
                        aligned_col_width(s, table, c, title, alignment)
                    }
                })
                .collect();
            return Some(TableLayout { columns, widths });
        }

        // Detail columns disappear from the right edge first. BRANCH sits
        // immediately after TITLE in every PR table, so it survives all other
        // optional metadata and is the last optional column removed.
        if let Some(c) = ((title + 1)..table.header.len())
            .rev()
            .find(|&c| shown[c] && Some(c) != branch)
        {
            if matches!(table.header[c], "FAIL" | "RUN" | "PASS") {
                for (column, header) in table.header.iter().enumerate() {
                    if matches!(*header, "FAIL" | "RUN" | "PASS") {
                        shown[column] = false;
                    }
                }
            } else {
                shown[c] = false;
            }
        } else {
            let c = branch.filter(|&c| shown[c])?;
            shown[c] = false;
        }
    }
}

/// Shared left-column widths for a set of section tables. Every table uses the
/// widest gutter and PR columns. When BRANCH is present, TITLE also uses one
/// shared width so every branch starts at the same cell.
pub fn table_alignment(s: &impl TextSurface, tables: &[&Table]) -> TableAlignment {
    let prefix_len = tables
        .iter()
        .filter_map(|table| table.column("TITLE"))
        .max()
        .unwrap_or(0);
    let prefix_widths = (0..prefix_len)
        .map(|column| {
            tables
                .iter()
                .filter(|table| column < table.header.len())
                .map(|table| col_width(s, table, column))
                .max()
                .unwrap_or(0)
        })
        .collect();
    let mut alignment = TableAlignment {
        prefix_widths,
        title_width: None,
        hide_branch: false,
    };
    let layouts: Vec<Option<TableLayout>> = tables
        .iter()
        .map(|table| table_layout(s, table, Some(&alignment)))
        .collect();
    if !tables.iter().any(|table| table.column("BRANCH").is_some()) {
        return alignment;
    }
    alignment.hide_branch = tables.iter().zip(&layouts).any(|(table, layout)| {
        let has_branch = table.column("BRANCH").is_some();
        has_branch
            && !layout.as_ref().is_some_and(|layout| {
                layout
                    .columns
                    .iter()
                    .any(|&column| table.header[column] == "BRANCH")
            })
    });
    if alignment.hide_branch {
        return alignment;
    }
    alignment.title_width = tables
        .iter()
        .zip(layouts)
        .filter_map(|(table, layout)| {
            let title = table.column("TITLE")?;
            let layout = layout?;
            let position = layout.columns.iter().position(|&column| column == title)?;
            Some(layout.widths[position])
        })
        .min();
    alignment
}

/// Whether the mandatory columns of `table` fit on `s` after every optional
/// column has been hidden.
pub fn table_fits(s: &impl TextSurface, table: &Table) -> bool {
    let alignment = table_alignment(s, &[table]);
    table_fits_aligned(s, table, &alignment)
}

pub fn table_fits_aligned(s: &impl TextSurface, table: &Table, alignment: &TableAlignment) -> bool {
    table_layout(s, table, Some(alignment)).is_some()
}

pub fn table_required_width(
    s: &impl TextSurface,
    table: &Table,
    alignment: &TableAlignment,
) -> u16 {
    let Some(title) = table.column("TITLE") else {
        let width = (0..table.header.len())
            .map(|column| col_width(s, table, column))
            .sum::<usize>()
            + SEP * table.header.len().saturating_sub(1);
        return width.min(usize::from(u16::MAX)) as u16;
    };
    let prefix = (0..title)
        .map(|column| aligned_col_width(s, table, column, title, Some(alignment)))
        .sum::<usize>();
    (prefix + SEP * title + TITLE_MIN).min(usize::from(u16::MAX)) as u16
}

/// Whether a wider surface would reveal a hidden column or more text in TITLE
/// or BRANCH.
pub fn table_is_compact(s: &impl TextSurface, table: &Table) -> bool {
    let alignment = table_alignment(s, &[table]);
    table_is_compact_aligned(s, table, &alignment)
}

pub fn table_is_compact_aligned(
    s: &impl TextSurface,
    table: &Table,
    alignment: &TableAlignment,
) -> bool {
    table_layout(s, table, Some(alignment)).is_some_and(|layout| {
        layout.columns.len() < table.header.len()
            || layout
                .columns
                .iter()
                .zip(&layout.widths)
                .any(|(&column, &width)| {
                    matches!(table.header[column], "TITLE" | "BRANCH")
                        && col_width(s, table, column) > width
                })
    })
}

/// Paint `table` onto `s` starting at row `top`. Columns are left-aligned and
/// separated by two blank columns; the header row is bold. Returns the next
/// free row.
pub fn paint_table(s: &mut impl TextSurface, table: &Table, ascii: bool, top: u16) -> u16 {
    let alignment = table_alignment(s, &[table]);
    paint_table_aligned(s, table, &alignment, ascii, top)
}

pub fn paint_table_aligned(
    s: &mut impl TextSurface,
    table: &Table,
    alignment: &TableAlignment,
    ascii: bool,
    top: u16,
) -> u16 {
    let Some(layout) = table_layout(s, table, Some(alignment)) else {
        return top;
    };
    let title_idx = table.column("TITLE");
    let branch_idx = table.column("BRANCH");

    // Column start positions: running sum of widths plus the separators.
    let mut xs = Vec::with_capacity(layout.columns.len());
    let mut acc = 0usize;
    for width in &layout.widths {
        xs.push(acc as u16);
        acc += width + SEP;
    }

    let bold = Style::new().bold();
    for (i, &column) in layout.columns.iter().enumerate() {
        let h = table.header[column];
        if !h.is_empty() {
            s.set_str((xs[i], top), h, &bold);
        }
    }

    for (r, row) in table.rows.iter().enumerate() {
        let y = top + 1 + r as u16;
        for (i, &column) in layout.columns.iter().enumerate() {
            let Some(cell) = row.get(column) else {
                continue;
            };
            let text = if Some(column) == title_idx || Some(column) == branch_idx {
                Cow::Owned(truncate(&cell.text, layout.widths[i], ascii))
            } else {
                Cow::Borrowed(cell.text.as_str())
            };
            s.set_str((xs[i], y), text.as_ref(), &cell.style);
        }
    }
    top + 1 + table.rows.len() as u16
}

/// Paint `table` alone onto an offscreen buffer and encode it to a string,
/// either styled (SGR + OSC-8) or plain. Plain output implies ASCII mode, as it
/// does for the dashboard. A convenience for checking a single section's output
/// without standing up a whole dashboard.
#[must_use]
pub fn render_table(table: &Table, styled: bool) -> String {
    let height = table.rows.len() as u16 + 1;
    let mut buf = TextBuffer::new(OUTPUT_WIDTH as u16, height);
    paint_table(&mut buf, table, !styled, 0);
    let mut out = Vec::new();
    let profile = if styled {
        Profile::TrueColor
    } else {
        Profile::Disabled
    };
    buf.encode_with(&mut out, profile)
        .expect("encoding to a Vec cannot fail");
    String::from_utf8(out).expect("uncurses encodes valid UTF-8")
}

/// Paint a dim one-liner (the error line, `Loading...`) at row `y`, flush left.
/// Returns y + 1.
pub fn paint_dim(s: &mut impl TextSurface, msg: &str, y: u16) -> u16 {
    paint_dim_at(s, msg, 0, y)
}

/// Paint a dim one-liner indented by `indent` columns. Returns y + 1.
pub fn paint_dim_at(s: &mut impl TextSurface, msg: &str, indent: u16, y: u16) -> u16 {
    s.set_str((indent, y), msg, Style::new().faint());
    y + 1
}

/// Paint a section header at row `y`: a colored bold accent bar, the title, an
/// optional dim count badge (or `Title (count)` in ASCII mode), and an optional
/// dim trailing note (e.g. the merge-queue ETA). Returns y + 1.
pub fn paint_header(
    s: &mut impl TextSurface,
    title: &str,
    accent: Color,
    count: Option<&str>,
    note: Option<&str>,
    ascii: bool,
    y: u16,
) -> u16 {
    let dim = Style::new().faint();
    let end = if ascii {
        let text = match count {
            Some(c) => format!("{title} ({c})"),
            None => title.to_string(),
        };
        s.set_str((0, y), &text, None)
    } else {
        let end = s.set_str(
            (0, y),
            &format!("\u{258c} {title}"),
            status::fg(accent).bold(),
        );
        match count {
            Some(c) => s.set_str((end.x + 2, y), c, &dim),
            None => end,
        }
    };
    if let Some(n) = note {
        s.set_str((end.x + 2, y), n, &dim);
    }
    y + 1
}

/// Where a table row's PR column starts: the (blank) marker cell and the
/// per-row glyph cell, each one column wide, plus their two-space separators.
/// Empty-section placeholders are indented by it so they read as a row.
pub const ROW_INDENT: u16 = 6;

/// A leading cell marking a row that changed since the previous refresh.
pub fn change_marker(highlighted: bool, ascii: bool) -> Cell {
    if highlighted {
        let m = if ascii { ">" } else { "\u{25b8}" };
        Cell::styled(m, status::fg(status::PINK).bold())
    } else {
        Cell::plain(" ")
    }
}

/// Paint the selection background across the whole of row `y` — edge to edge,
/// not just the painted text — so the row the navigation cursor is on reads as
/// one solid bar. It replaces a caret glyph, leaving the leading column free, so
/// a row that is both changed and selected shows both. Only the background is
/// set: every cell keeps its text, color and link. Runs after the body is
/// painted, so it covers a hand-laid-out section (the shipments) exactly as it
/// covers the tables.
pub fn highlight_row(s: &mut impl TextSurface, y: u16) {
    let b = s.bounds();
    for x in b.x..b.x + b.width {
        if let Some(cell) = s.cell_mut(Position::new(x, y)) {
            cell.style.bg = Some(status::SURFACE);
        }
    }
}

/// Paint the watch-mode key-hint footer at row `y`, folding the constant
/// refresh interval into the refresh hint: `r refresh (every 5m) - tab switch
/// view - enter open - / search - ? help`. While a refresh is in flight the
/// refresh hint becomes `r refreshing` (the interval is dropped and the `r` glyph
/// is dimmed, since `r` is inert until the fetch finishes). Each key glyph is a
/// bold muted accent, its labels dim; plain in ASCII mode. Returns y + 1.
pub fn paint_footer(
    s: &mut impl TextSurface,
    interval: &str,
    refreshing: bool,
    more: bool,
    ascii: bool,
    y: u16,
) -> u16 {
    let refresh = if refreshing {
        "refreshing".to_string()
    } else {
        format!("refresh (every {interval})")
    };
    let mut hints = vec![
        ("r", refresh),
        ("tab", "switch view".to_string()),
        ("enter", "open".to_string()),
        ("y", "copy".to_string()),
        ("/", "search".to_string()),
        ("?", "help".to_string()),
    ];
    if more {
        hints.insert(0, ("+", "resize for more".to_string()));
    }

    let available = usize::from(s.bounds().width);
    let width = |hints: &[(&str, String)]| {
        hints
            .iter()
            .map(|(key, label)| {
                s.str_width(key) as usize
                    + usize::from(!label.is_empty())
                    + s.str_width(label) as usize
            })
            .sum::<usize>()
            + 3 * hints.len().saturating_sub(1)
    };
    let essential = 1 + usize::from(more);
    if width(&hints) > available {
        for index in (essential..hints.len()).rev() {
            hints[index].1.clear();
            if width(&hints) <= available {
                break;
            }
        }
    }
    while width(&hints) > available && hints.len() > essential {
        hints.pop();
    }
    if width(&hints) > available {
        if more {
            hints[0].1 = "more".to_string();
        }
        if !refreshing {
            hints[usize::from(more)].1 = "refresh".to_string();
        }
    }
    if width(&hints) > available && more {
        hints[0].1.clear();
    }
    if width(&hints) > available && !refreshing {
        hints[usize::from(more)].1.clear();
    }

    let text = |key: &str, label: &str| {
        if label.is_empty() {
            key.to_string()
        } else {
            format!("{key} {label}")
        }
    };
    if ascii {
        let line = hints
            .iter()
            .map(|(key, label)| text(key, label))
            .collect::<Vec<_>>()
            .join(" - ");
        s.set_str((0, y), &line, None);
        return y + 1;
    }
    let key = status::fg(status::OVERLAY).bold();
    let dim = Style::new().faint();
    let mut x = 0u16;
    for (i, (key_glyph, label)) in hints.iter().enumerate() {
        if i > 0 {
            x = s.set_str((x, y), " - ", &dim).x;
        }
        // `r` is inert while a fetch is in flight, so its glyph fades to dim.
        let key_style = if *key_glyph == "r" && refreshing {
            &dim
        } else {
            &key
        };
        let p = s.set_str((x, y), key_glyph, key_style);
        x = if label.is_empty() {
            p.x
        } else {
            s.set_str((p.x + 1, y), label, &dim).x
        };
    }
    y + 1
}

/// Paint the view switcher at row `y`: both view names, the active one accented
/// with a section-style bar, the other dim (the active one is bracketed in ASCII
/// mode). Returns y + 1.
pub fn paint_tabs(s: &mut impl TextSurface, view: View, ascii: bool, y: u16) -> u16 {
    let names = [(View::Mine, "my PRs"), (View::Reviews, "reviews")];
    let bar = status::fg(status::LAVENDER).bold();
    let dim = Style::new().faint();
    let mut x = 0u16;
    for (v, n) in names {
        let active = v == view;
        x = if ascii {
            let text = if active {
                format!("[{n}]")
            } else {
                n.to_string()
            };
            s.set_str((x, y), &text, None).x
        } else if active {
            s.set_str((x, y), &format!("\u{258c}{n}"), &bar).x
        } else {
            s.set_str((x, y), n, &dim).x
        };
        x += 2;
    }
    y + 1
}

/// Paint the search prompt at row `y`: an accented `/`, the query, and a dim
/// match count. Returns the next free row and the cell just past the query — the
/// caret position, for the caller to park the terminal's own cursor on while the
/// prompt is capturing (this paints no cursor of its own).
pub fn paint_search_prompt(
    s: &mut impl TextSurface,
    query: &str,
    matches: usize,
    ascii: bool,
    y: u16,
) -> (u16, Position) {
    let count = if matches == 1 {
        "1 match".to_string()
    } else {
        format!("{matches} matches")
    };
    let prompt = format!("/{query}");
    let style = if ascii {
        Style::new()
    } else {
        status::fg(status::PEACH).bold()
    };
    let caret = s.set_str((0, y), &prompt, style);
    // Two blank columns keep the count clear of the cursor's cell.
    s.set_str(
        (caret.x + 2, y),
        &format!("({count})"),
        Style::new().faint(),
    );
    (y + 1, caret)
}

/// Compose one screen of exactly `rows` rows: as much of `body` as fits at the
/// top, then `bottom` glued to the last rows. When the body is taller than the
/// space left over it scrolls, keeping `caret` (a row index into `body`) centered
/// in view. `screen` is assumed already cleared, so the gap between the two is
/// blank padding.
///
/// Returns the row the bottom block starts on and how many rows were cut from
/// its head, which callers need to translate a position inside it (the search
/// caret) into screen coordinates: bottom row `y` lands on `top + y - cut`, and
/// a row above `cut` was not drawn at all.
pub fn compose<T: SurfaceMut + Bounded + ?Sized>(
    screen: &mut T,
    body: &mut TextBuffer,
    body_h: u16,
    bottom: &mut TextBuffer,
    bottom_h: u16,
    rows: u16,
    caret: Option<u16>,
) -> (u16, u16) {
    // The bottom block wins the space it needs; if it alone overflows (a short
    // terminal with the help legend open) it keeps its tail — the search prompt,
    // error line and footer — and the legend above them is what gets cut.
    let shown_bottom = bottom_h.min(rows);
    let cut = bottom_h - shown_bottom;
    let avail = rows - shown_bottom;

    // Centering the caret scrolls the body a row at a time in either direction;
    // with no caret (or nothing to scroll) we sit at the top.
    let off = match caret {
        Some(c) if body_h > avail => c.saturating_sub(avail / 2).min(body_h - avail),
        _ => 0,
    };
    if avail > 0 {
        // A `View` clips without translating, so drawing it maps its top-left —
        // the first visible body row — onto the top of the screen.
        let w = body.width();
        BufView::new(body, Rect::new(0, off, w, avail)).draw(screen, Position::new(0, 0));
    }
    let top = rows - shown_bottom;
    let bw = bottom.width();
    BufView::new(bottom, Rect::new(0, cut, bw, shown_bottom)).draw(screen, Position::new(0, top));
    (top, cut)
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

/// Paint the help legend for `view` at row `top`: the navigation keys, then only
/// the glyphs and values that view actually uses, so a glyph the other view
/// reuses for something else can't muddy it. The Mine view lists the approval
/// glyphs and the conflict marker; the Reviews view lists review-state glyphs.
/// Painted above the search prompt / footer, since it documents the keys they
/// list. Returns the next free row.
pub fn paint_help(s: &mut impl TextSurface, view: View, ascii: bool, top: u16) -> u16 {
    let dim = Style::new().faint();
    let mut y = paint_header(s, "Help", status::OVERLAY, None, None, ascii, top);

    // The footer only lists the action keys, so document the movement cursor here.
    let sep = if ascii { " | " } else { "  \u{b7}  " };
    let keys = format!(
        "j/k move{sep}g/G first/last{sep}^D/^U half page{sep}enter open{sep}y copy link{sep}Y copy section{sep}/ filter"
    );
    s.set_str((2, y), &keys, &dim);
    y += 1;

    match view {
        View::Mine => {
            for a in status::APPROVAL_ORDER {
                let glyph = status::approval_glyph(a, ascii).to_string();
                let color = status::fg(status::approval_style(a).1);
                legend_row(s, &glyph, color, status::approval_meaning(a), &dim, y);
                y += 1;
            }
            let marker = status::conflict_marker(ascii).to_string();
            legend_row(
                s,
                &marker,
                status::fg(status::RED),
                status::CONFLICT_MEANING,
                &dim,
                y,
            );
            y += 1;
        }
        View::Reviews => {
            for r in status::REVIEW_ORDER {
                let glyph = status::review_glyph(r, ascii).to_string();
                let color = status::fg(status::review_style(r).1);
                legend_row(s, &glyph, color, status::review_meaning(r), &dim, y);
                y += 1;
            }
        }
    }
    y
}

/// The number of legend rows [`paint_help`] paints for `view` (header + the key
/// line + one row per entry), so callers can size a surface before painting.
pub fn help_height(view: View) -> usize {
    2 + match view {
        // One approval glyph per state, plus the conflict marker.
        View::Mine => status::APPROVAL_ORDER.len() + 1,
        View::Reviews => status::REVIEW_ORDER.len(),
    }
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

    /// Paint one text row per line of `lines` into a buffer of that height.
    fn buf(lines: &[&str]) -> (TextBuffer, u16) {
        let mut b = TextBuffer::new(8, lines.len().max(1) as u16);
        for (y, l) in lines.iter().enumerate() {
            b.set_str((0, y as u16), l, None);
        }
        (b, lines.len() as u16)
    }

    /// Compose onto a `rows`-tall screen and read the result back as trimmed rows.
    fn screen_rows(body: &[&str], bottom: &[&str], rows: u16, caret: Option<u16>) -> Vec<String> {
        let (mut b, bh) = buf(body);
        let (mut bot, both) = buf(bottom);
        let mut screen = TextBuffer::new(8, rows);
        compose(&mut screen, &mut b, bh, &mut bot, both, rows, caret);
        let mut out: Vec<String> = screen
            .display_with(Profile::Disabled)
            .to_string()
            .lines()
            .map(|l| l.trim_end().to_string())
            .collect();
        // The display drops trailing all-blank rows; the buffer still has them.
        out.resize(rows as usize, String::new());
        out
    }

    #[test]
    fn compose_pins_the_bottom_block_to_the_last_rows() {
        // A short body is padded out so the footer lands on the last row.
        assert_eq!(
            screen_rows(&["a", "b"], &["footer"], 6, None),
            ["a", "b", "", "", "", "footer"]
        );
    }

    #[test]
    fn compose_scrolls_the_body_to_keep_the_caret_in_view() {
        let body: Vec<String> = (0..20).map(|i| i.to_string()).collect();
        let body: Vec<&str> = body.iter().map(String::as_str).collect();
        // 6 rows, one of them the footer, leaves 5 for the body.
        let five = |caret| screen_rows(&body, &["footer"], 6, caret);

        // With no selection the body starts at the top...
        assert_eq!(five(None), ["0", "1", "2", "3", "4", "footer"]);
        // ...a caret past the fold scrolls into view, centered...
        assert_eq!(five(Some(10)), ["8", "9", "10", "11", "12", "footer"]);
        // ...and the last row can't scroll past the end of the body.
        assert_eq!(five(Some(19)), ["15", "16", "17", "18", "19", "footer"]);
    }

    #[test]
    fn compose_reports_the_rows_cut_from_the_bottom_block() {
        let (mut b, bh) = buf(&["body"]);
        let mut at = |rows| {
            let (mut bot, both) = buf(&["help", "search", "footer"]);
            let mut screen = TextBuffer::new(8, rows);
            compose(&mut screen, &mut b, bh, &mut bot, both, rows, None)
        };
        // Room for everything: nothing is cut, so a position in the block maps
        // straight onto `top + y`.
        assert_eq!(at(5), (2, 0));
        // Too short: the head goes. The search prompt (block row 1) is still
        // drawn, now at screen row `top + 1 - cut`.
        let (top, cut) = at(2);
        assert_eq!((top, cut), (0, 1));
        assert_eq!(top + (1 - cut), 0);
        // Shorter still: only the footer survives and the prompt is gone, which
        // the caller detects as `y < cut`.
        let (_, cut) = at(1);
        assert_eq!(cut, 2);
        assert!(1 < cut);
    }

    #[test]
    fn compose_keeps_the_footer_when_the_bottom_block_overflows() {
        // Too short for both: the body goes, and the bottom block is cut from
        // the start so its last lines — down to the footer — stay.
        assert_eq!(
            screen_rows(&["a", "b"], &["h1", "h2", "footer"], 2, None),
            ["h2", "footer"]
        );
    }

    #[test]
    fn compose_never_panics_and_always_keeps_the_footer() {
        // Degenerate shapes must not panic and must never drop the tail of the
        // bottom block, where the footer lives.
        let bodies: [&[&str]; 4] = [&[], &["a"], &["a", "b", "c", "d", "e", "f"], &["x", "", ""]];
        let bottoms: [&[&str]; 3] = [&[], &["f"], &["h1", "h2", "h3", "f"]];
        for body in bodies {
            for bottom in bottoms {
                for rows in 1..12u16 {
                    for caret in [None, Some(0), Some(1), Some(5), Some(100)] {
                        let out = screen_rows(body, bottom, rows, caret);
                        assert_eq!(out.len(), rows as usize, "{body:?}/{bottom:?}/{rows}");
                        if let Some(footer) = bottom.last() {
                            assert!(out.contains(&(*footer).to_string()));
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn search_prompt_reports_the_caret_and_paints_none() {
        let mut canvas = TextBuffer::new(40, 1);
        let (next, caret) = paint_search_prompt(&mut canvas, "café", 1, false, 0);
        assert_eq!(next, 1);
        // Just past `/café` — width, not byte length.
        assert_eq!(caret.x, 5);
        let mut out = Vec::new();
        canvas.encode_with(&mut out, Profile::TrueColor).unwrap();
        let s = String::from_utf8(out).unwrap();
        assert!(s.contains("/café"));
        assert!(s.contains("(1 match)"));
        // No painted stand-in cursor: nothing is reverse-video.
        assert!(!s.contains("\x1b[7m"));
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
            paint_table(b, &table, true, 0);
        });
        // Header, then two rows; columns line up by display width, no escapes.
        assert_eq!(out, "PR   TITLE\r\n#1   short\r\n#42  x");
    }

    #[test]
    fn padding_uses_display_width_for_glyphs() {
        // The check glyph is one display column but several bytes; the
        // following column must still line up by display width.
        let glyph = status::approval_style(status::Approval::Approved).0;
        let table = Table {
            header: vec!["ST", "PR"],
            rows: vec![
                vec![Cell::plain(glyph.to_string()), Cell::plain("#1")],
                vec![Cell::plain("xx"), Cell::plain("#2")],
            ],
        };
        let out = encode(10, 3, Profile::Disabled, |b| {
            paint_table(b, &table, true, 0);
        });
        let lines: Vec<&str> = out.split("\r\n").collect();
        let col = |line: &str| line.find('#').map_or("", |i| &line[..i]).chars().count();
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
            paint_table(b, &table, false, 0);
        });
        // OSC-8 framing: the opener carries a per-URL `id=` param, the closer is
        // empty; dim + underline SGR.
        assert!(out.contains("\x1b]8;id="));
        assert!(out.contains(";https://x/1\x1b\\"));
        assert!(out.contains("\x1b]8;;\x1b\\"));
        assert!(out.contains("\x1b[2;4m"));
    }

    #[test]
    fn link_params_is_a_7_hex_char_id() {
        let p = link_params("https://github.com/o/r/pull/42");
        let id = p.strip_prefix("id=").expect("id= prefix");
        assert_eq!(id.len(), 7);
        assert!(id.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn disabled_profile_drops_styling_and_links() {
        let table = Table {
            header: vec!["URL"],
            rows: vec![vec![Cell::link("https://x/1", "https://x/1")]],
        };
        let out = encode(12, 2, Profile::Disabled, |b| {
            paint_table(b, &table, false, 0);
        });
        assert!(!out.contains('\x1b'));
        assert!(out.contains("https://x/1"));
    }

    #[test]
    fn footer_is_plain_or_styled_key_hints() {
        let plain = encode(80, 1, Profile::Disabled, |b| {
            paint_footer(b, "5m", false, false, true, 0);
        });
        assert_eq!(
            plain,
            "r refresh (every 5m) - tab switch view - enter open - y copy - / search - ? help"
        );

        // While a refresh is in flight the refresh hint says so instead.
        let refreshing = encode(80, 1, Profile::Disabled, |b| {
            paint_footer(b, "5m", true, false, true, 0);
        });
        assert!(refreshing.starts_with("r refreshing"));

        let styled = encode(80, 1, Profile::TrueColor, |b| {
            paint_footer(b, "5m", false, false, false, 0);
        });
        assert!(styled.contains("refresh (every 5m)"));
        assert!(styled.contains("help"));
        // Bold key accent (combined with the muted color) and a dim label.
        assert!(styled.contains("\x1b[1;"));
        assert!(styled.contains("\x1b[2m"));

        let compact = encode(80, 1, Profile::Disabled, |b| {
            paint_footer(b, "5m", false, true, true, 0);
        });
        assert!(compact.starts_with("+ resize for more - r refresh"));

        for width in MIN_WIDTH..80 {
            let plain = encode(width, 1, Profile::Disabled, |b| {
                paint_footer(b, "5m", false, false, true, 0);
            });
            assert!(plain.chars().count() <= usize::from(width));
            assert!(plain.starts_with('r'));

            let constrained = encode(width, 1, Profile::Disabled, |b| {
                paint_footer(b, "5m", false, true, true, 0);
            });
            assert!(constrained.chars().count() <= usize::from(width));
            assert!(constrained.starts_with('+'));
        }

        let compact_refreshing = encode(80, 1, Profile::TrueColor, |b| {
            paint_footer(b, "5m", true, true, false, 0);
        });
        assert!(compact_refreshing.starts_with("\x1b[1;"));
        assert!(compact_refreshing.contains("\x1b[2mresize for more - r\x1b[m \x1b[2mrefreshing"));
    }

    #[test]
    fn truncate_marks_cut_with_ellipsis() {
        assert_eq!(truncate("short", 10, false), "short");
        assert_eq!(truncate("hello world", 8, false), "hello w\u{22ef}");
        assert_eq!(truncate("hello world", 8, true), "hello...");
    }

    #[test]
    fn title_and_branch_fill_the_available_width() {
        let long = "x".repeat(200);
        let table = Table {
            header: vec!["", "PR", "TITLE", "BRANCH", "BASE"],
            rows: vec![vec![
                Cell::plain(" "),
                Cell::plain("#1"),
                Cell::plain(long),
                Cell::plain("feature/a-very-long-branch"),
                Cell::plain("main"),
            ]],
        };
        let mut canvas = TextBuffer::new(OUTPUT_WIDTH as u16, 2);
        paint_table(&mut canvas, &table, false, 0);
        let out = canvas.display_with(Profile::Disabled).to_string();
        for line in out.split("\r\n") {
            assert!(
                line.chars().count() <= OUTPUT_WIDTH,
                "line exceeds surface width"
            );
        }
        // The long title was truncated with an ellipsis.
        assert!(out.contains('\u{22ef}'));
    }

    #[test]
    fn narrow_tables_hide_detail_columns_before_branch() {
        let table = Table {
            header: vec!["", "PR", "TITLE", "BRANCH", "AUTHOR", "UPDATED"],
            rows: vec![vec![
                Cell::plain(" "),
                Cell::plain("#1"),
                Cell::plain("a title"),
                Cell::plain("feature/one"),
                Cell::plain("monalisa"),
                Cell::plain("2h"),
            ]],
        };

        let out = encode(24, 2, Profile::Disabled, |b| {
            paint_table(b, &table, true, 0);
        });
        assert!(out.contains("TITLE"));
        assert!(out.contains("BRANCH"));
        assert!(!out.contains("AUTHOR"));
        assert!(!out.contains("UPDATED"));
    }

    #[test]
    fn title_stays_larger_than_branch_at_minimum_width() {
        let table = Table {
            header: vec!["", "PR", "TITLE", "BRANCH"],
            rows: vec![vec![
                Cell::plain(" "),
                Cell::plain("#1"),
                Cell::plain("short"),
                Cell::plain("feature/a-long-branch"),
            ]],
        };
        let canvas = TextBuffer::new(MIN_WIDTH, 2);
        let layout = table_layout(&canvas, &table, None).expect("minimum width should fit");
        let title = layout
            .columns
            .iter()
            .position(|&column| table.header[column] == "TITLE")
            .expect("title column");
        let branch = layout
            .columns
            .iter()
            .position(|&column| table.header[column] == "BRANCH")
            .expect("branch column");
        assert!(layout.widths[title] > layout.widths[branch]);
    }

    #[test]
    fn mandatory_columns_can_report_terminal_too_small() {
        let table = Table {
            header: vec!["PR", "TITLE", "BRANCH"],
            rows: vec![vec![
                Cell::plain("#12345678901234567890"),
                Cell::plain("title"),
                Cell::plain("branch"),
            ]],
        };
        let canvas = TextBuffer::new(MIN_WIDTH, 2);
        assert!(!table_fits(&canvas, &table));
        let alignment = table_alignment(&canvas, &[&table]);
        assert!(table_required_width(&canvas, &table, &alignment) > MIN_WIDTH);
    }

    #[test]
    fn alignment_keeps_pr_title_and_branch_starts_equal() {
        let table =
            |marker: &'static str, number: &str, title: &str, branch: &str, tail: &'static str| {
                Table {
                    header: vec!["", marker, "PR", "TITLE", "BRANCH", tail],
                    rows: vec![vec![
                        Cell::plain(" "),
                        Cell::plain(" "),
                        Cell::plain(number),
                        Cell::plain(title),
                        Cell::plain(branch),
                        Cell::plain("x"),
                    ]],
                }
            };
        let first = table("", "#1", "first title", "b/first", "FAIL");
        let second = table("#", "#200", "second title", "b/second", "AUTHOR");
        let third = table("", "#30", "third title", "b/third", "MERGED");
        let mut canvas = TextBuffer::new(80, 6);
        let alignment = table_alignment(&canvas, &[&first, &second, &third]);
        paint_table_aligned(&mut canvas, &first, &alignment, true, 0);
        paint_table_aligned(&mut canvas, &second, &alignment, true, 2);
        paint_table_aligned(&mut canvas, &third, &alignment, true, 4);

        let output = canvas.display_with(Profile::Disabled).to_string();
        let lines: Vec<&str> = output.lines().collect();
        let starts = |line: &str, values: [&str; 3]| {
            values.map(|value| line.find(value).expect("painted cell"))
        };
        assert_eq!(
            starts(lines[1], ["#1", "first title", "b/first"]),
            starts(lines[3], ["#200", "second title", "b/second"])
        );
        assert_eq!(
            starts(lines[1], ["#1", "first title", "b/first"]),
            starts(lines[5], ["#30", "third title", "b/third"])
        );
    }

    #[test]
    fn alignment_keeps_the_check_semaphore_at_the_right() {
        let open = Table {
            header: vec!["", "", "PR", "TITLE", "THREADS", "FAIL", "RUN", "PASS"],
            rows: vec![],
        };
        let queue = Table {
            header: vec![
                "", "#", "PR", "TITLE", "AUTHOR", "WAIT", "BUILD", "FAIL", "RUN", "PASS",
            ],
            rows: vec![],
        };
        let mut canvas = TextBuffer::new(80, 2);
        let alignment = table_alignment(&canvas, &[&open, &queue]);
        paint_table_aligned(&mut canvas, &open, &alignment, true, 0);
        paint_table_aligned(&mut canvas, &queue, &alignment, true, 1);

        let output = canvas.display_with(Profile::Disabled).to_string();
        let lines: Vec<&str> = output.lines().collect();
        let starts = |line: &str| {
            ["FAIL", "RUN", "PASS"].map(|value| line.find(value).expect("semaphore column"))
        };
        assert_eq!(starts(lines[0]), starts(lines[1]));
    }

    #[test]
    fn responsive_layout_hides_the_whole_check_semaphore() {
        let table = Table {
            header: vec!["", "PR", "TITLE", "THREADS", "FAIL", "RUN", "PASS"],
            rows: vec![vec![
                Cell::plain(" "),
                Cell::plain("#1"),
                Cell::plain("a title"),
                Cell::plain("0"),
                Cell::plain("1"),
                Cell::plain("2"),
                Cell::plain("3"),
            ]],
        };
        for width in MIN_WIDTH..=80 {
            let output = encode(width, 2, Profile::Disabled, |canvas| {
                paint_table(canvas, &table, true, 0);
            });
            let shown = ["FAIL", "RUN", "PASS"].map(|header| output.contains(header));
            assert!(
                shown.iter().all(|&value| value == shown[0]),
                "partial semaphore at width {width}: {shown:?}"
            );
        }
    }
}
