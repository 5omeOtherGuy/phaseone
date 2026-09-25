//! BLOCK transcript grammar. All coordinates are display cells, including padding.
//! A tool's argument yields space to its outcome; output never soft wraps.
//!
//! Layout is lazy (Iris pager idea, extended to widths): every block's height is
//! measured cheaply and kept, styled rows are built only for the blocks a frame
//! shows. Tool bodies never wrap, so a tool block's height does not depend on the
//! width — a resize re-measures prose only, never the whole styled history.
use crate::{
    fold::FoldId,
    palette as p,
    transcript::{Block, RowStatus, ToolRow, Transcript},
    wrap::{cell_width, fit_cells},
};
use p1_contracts::ToolStatus;
use ratatui::{
    style::{Color, Style},
    text::{Line, Span},
};

/// Rows kept in a folded preview, and the size above which a body folds (§5).
const FOLD_AT: usize = 40;
const FOLD_KEEP: usize = 24;
const WRITE_KEEP: usize = 6;

/// Text as copied: terminal escapes (colour, OSC titles) removed, tabs and
/// line breaks kept exactly — what the operator would paste elsewhere.
pub fn strip_escapes(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            match chars.next() {
                Some('[') => {
                    for c in chars.by_ref() {
                        if ('@'..='~').contains(&c) {
                            break;
                        }
                    }
                }
                Some(']' | 'P' | '_' | '^' | 'X') => {
                    while let Some(c) = chars.next() {
                        if c == '\x07' || (c == '\x1b' && chars.next_if_eq(&'\\').is_some()) {
                            break;
                        }
                    }
                }
                _ => {}
            }
        } else if c == '\t' || c == '\n' || !c.is_control() {
            out.push(c);
        }
    }
    out
}

pub fn clean(text: &str) -> String {
    let mut out = String::new();
    let mut chars = text.chars().peekable();
    let mut col = 0;
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            match chars.next() {
                Some('[') => {
                    for c in chars.by_ref() {
                        if ('@'..='~').contains(&c) {
                            break;
                        }
                    }
                }
                Some(']' | 'P' | '_' | '^' | 'X') => {
                    while let Some(c) = chars.next() {
                        if c == '\x07' || (c == '\x1b' && chars.next_if_eq(&'\\').is_some()) {
                            break;
                        }
                    }
                }
                _ => {}
            }
        } else if c == '\t' {
            let n = 8 - col % 8;
            out.push_str(&" ".repeat(n));
            col += n;
        } else if !c.is_control() {
            out.push(c);
            col += unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
        }
    }
    out
}

pub fn ellipsis(text: &str, width: usize) -> String {
    let text = clean(text);
    if cell_width(&text) <= width {
        return text;
    }
    if width == 0 {
        return String::new();
    }
    format!("{}…", fit_cells(&text, width - 1))
}

/// A byte count as the outcome shows it: `37 B`, `4.2 kB`, `1.3 MB`.
pub fn size(bytes: usize) -> String {
    if bytes < 1_000 {
        format!("{bytes} B")
    } else if bytes < 1_000_000 {
        format!("{:.1} kB", bytes as f64 / 1_000.0)
    } else {
        format!("{:.1} MB", bytes as f64 / 1_000_000.0)
    }
}

/// `1 line`, `2 lines`: counts are facts and read as such.
pub fn plural(n: usize, word: &str) -> String {
    if n == 1 {
        format!("1 {word}")
    } else {
        format!("{n} {word}s")
    }
}

/// Clip styled cells, never losing the background at the right edge.
pub fn band(spans: Vec<Span<'static>>, width: usize, bg: Color) -> Line<'static> {
    let mut out = vec![];
    let mut used = 0;
    for span in spans {
        let text = fit_cells(&span.content, width.saturating_sub(used));
        used += cell_width(&text);
        out.push(Span::styled(
            text,
            span.style.bg(span.style.bg.unwrap_or(bg)),
        ));
    }
    out.push(Span::styled(
        " ".repeat(width.saturating_sub(used)),
        Style::new().bg(bg),
    ));
    Line::from(out).style(Style::new().bg(bg))
}

pub fn padded(spans: Vec<Span<'static>>, width: usize, bg: Color) -> Line<'static> {
    let inner = band(spans, width.saturating_sub(4), bg);
    let mut spans = vec![Span::raw(" ".repeat(width.min(2)))];
    spans.extend(inner.spans);
    band(spans, width, bg)
}

pub fn body(text: &str, width: usize, fg: Color, bg: Color) -> Line<'static> {
    let text = clean(text);
    let u = width.saturating_sub(4);
    let spans = clipped(&text, u, fg);
    padded(spans, width, bg)
}

fn clipped(text: &str, width: usize, fg: Color) -> Vec<Span<'static>> {
    if cell_width(text) <= width {
        return vec![Span::styled(text.to_owned(), Style::new().fg(fg))];
    }
    if width == 0 {
        return vec![];
    }
    let cut = fit_cells(text, width - 1);
    let pad = width - 1 - cell_width(&cut);
    vec![
        Span::styled(format!("{cut}{}", " ".repeat(pad)), Style::new().fg(fg)),
        Span::styled("›", Style::new().fg(p::FAINT)),
    ]
}

/// Drop trailing ` · fact` segments until the outcome leaves the argument room
/// to stay identifiable; the mark and the first fact always survive.
fn fit_outcome(outcome: &str, room: usize) -> String {
    let mut outcome = clean(outcome);
    // Secondary facts go first (` · …`), then trailing words: `✓ 12 lines
    // written` → `✓ 12 lines` → `✓`, so the argument keeps its cells.
    while cell_width(&outcome) > room {
        match outcome.rfind(" · ") {
            Some(at) if outcome[..at].contains(' ') => outcome.truncate(at),
            _ => break,
        }
    }
    while cell_width(&outcome) > room {
        match outcome.trim_end().rfind(' ') {
            Some(at) if at > 0 => outcome.truncate(at),
            _ => break,
        }
    }
    outcome
}

pub fn header(name: &str, arg: &str, outcome: &str, width: usize, blocking: bool) -> Line<'static> {
    let u = width.saturating_sub(4);
    // The argument keeps at least a few cells: a path that vanishes entirely
    // is worse than an outcome that loses its secondary facts.
    let outcome = fit_outcome(outcome, u.saturating_sub(12 + 2 + 8.min(cell_width(arg))));
    let name = ellipsis(name, 10);
    let arg_room = u.saturating_sub(12 + 2 + cell_width(&outcome));
    let arg = if arg_room == 0 && !arg.is_empty() {
        String::new()
    } else {
        ellipsis(arg, arg_room)
    };
    let used = 12 + cell_width(&arg) + cell_width(&outcome);
    let mut spans = vec![
        Span::styled(
            if blocking { "! " } else { "▸ " },
            Style::new().fg(if blocking { p::INK } else { p::DIM }),
        ),
        Span::styled(
            format!("{name}{}", " ".repeat(10 - cell_width(&name))),
            Style::new().fg(p::DIM),
        ),
        Span::styled(arg, Style::new().fg(p::INK)),
        Span::raw(" ".repeat(u.saturating_sub(used))),
    ];
    if let Some(rest) = outcome.strip_prefix('✗') {
        spans.push(Span::styled("✗", Style::new().fg(p::INK)));
        spans.push(Span::styled(rest.to_owned(), Style::new().fg(p::DIM)));
    } else {
        spans.push(Span::styled(outcome, Style::new().fg(p::DIM)));
    }
    padded(spans, width, p::BLOCK_PLUS)
}

pub fn numbered(
    number: usize,
    digits: usize,
    sign: Option<char>,
    text: &str,
    width: usize,
) -> Line<'static> {
    let (fg, bg) = match sign {
        Some('+') => (p::DIFF_ADD_FG, p::DIFF_ADD_BG),
        Some('-' | '−') => (p::DIFF_DEL_FG, p::DIFF_DEL_BG),
        _ => (p::DIM, p::BLOCK),
    };
    let digits = digits.max(3);
    // On a hued row the number takes the row's own foreground: FAINT on the
    // hue would fall to about 2:1 contrast.
    let number_fg = if matches!(sign, Some('+' | '-' | '−')) {
        fg
    } else {
        p::FAINT
    };
    let mut spans = vec![Span::styled(
        if number == 0 {
            " ".repeat(digits + 2)
        } else {
            format!("{number:>digits$}  ")
        },
        Style::new().fg(number_fg),
    )];
    if let Some(sign) = sign {
        spans.push(Span::styled(format!("{sign} "), Style::new().fg(fg)));
    }
    let prefix = digits + 2 + if sign.is_some() { 2 } else { 0 };
    spans.extend(clipped(&clean(text), width.saturating_sub(4 + prefix), fg));
    padded(spans, width, bg)
}

pub fn meta(left: &str, right: &str, width: usize) -> Line<'static> {
    meta_styled(left, p::FAINT, right, width)
}

fn meta_styled(left: &str, fg: Color, right: &str, width: usize) -> Line<'static> {
    let u = width.saturating_sub(4);
    let right = if cell_width(right) + 2 + cell_width(left) <= u {
        right
    } else {
        ""
    };
    let left = ellipsis(left, u.saturating_sub(cell_width(right)));
    let pad = u.saturating_sub(cell_width(&left) + cell_width(right));
    padded(
        vec![
            Span::styled(left, Style::new().fg(fg)),
            Span::raw(" ".repeat(pad)),
            Span::styled(right.to_owned(), Style::new().fg(p::FAINT)),
        ],
        width,
        p::BLOCK,
    )
}

/// The fold row: the handle is the one thing that must survive any width, so
/// the wording shrinks first (§5: the id is addressable).
fn fold_meta(
    more: usize,
    id: &FoldId,
    expanded: bool,
    width: usize,
    omitted: Option<&str>,
) -> Line<'static> {
    let u = width.saturating_sub(4);
    let handle = format!("[{id}]");
    let candidates = if expanded {
        vec![
            format!("· all {} shown → {handle}", plural(more, "line")),
            format!("· all shown → {handle}"),
            handle.clone(),
        ]
    } else {
        let folded = format!(
            "· {} more {} folded",
            more,
            if more == 1 { "line" } else { "lines" }
        );
        let mut c = vec![];
        // The tool itself dropped bytes: say so, the pane cannot show them.
        if let Some(omitted) = omitted {
            c.push(format!(
                "{folded} · {omitted} omitted by the tool → {handle}"
            ));
            c.push(format!("{folded} · {omitted} omitted → {handle}"));
        }
        c.push(format!("{folded} → {handle}"));
        c.push(format!("· {more} more → {handle}"));
        c.push(handle.clone());
        c
    };
    let right = if expanded {
        "click header to fold"
    } else {
        "^O open in pane"
    };
    // The longest wording that still leaves room for the key hint; without
    // room for both, the handle and what it holds come first.
    // The bare handle never pushes the count out for the hint's sake.
    let counted = &candidates[..candidates.len() - 1];
    let left = counted
        .iter()
        .find(|c| cell_width(c) + 3 + cell_width(right) <= u)
        .or_else(|| candidates.iter().find(|c| cell_width(c) <= u))
        .cloned()
        .unwrap_or(handle);
    meta(&left, right, width)
}

pub fn label(label: &str, value: &str, width: usize) -> Line<'static> {
    let label = ellipsis(label, 9);
    let mut spans = vec![Span::styled(
        format!("{label}{}", " ".repeat(9 - cell_width(&label))),
        Style::new().fg(p::DIM),
    )];
    spans.extend(clipped(&clean(value), width.saturating_sub(13), p::INK));
    padded(spans, width, p::BLOCK)
}

/// Shared by permission and diff review; unavailable grants never invert.
pub fn decisions(grantable: bool, width: usize) -> Vec<Line<'static>> {
    decisions_with(grantable, width, "")
}

/// The decision row(s): inverted chips for what can be granted, FAINT for what
/// cannot (`p project` until a trust store persists grants). Rows wrap before
/// anything is cut, and `n deny` is always on the first row. `note` sits
/// right-aligned FAINT when it fits.
pub fn decisions_with(grantable: bool, width: usize, note: &str) -> Vec<Line<'static>> {
    let u = width.saturating_sub(4);
    // Ungrantable decisions get their own rows with the reason (below); `p`
    // is shown unavailable in the row until a trust store persists grants.
    let chips: Vec<(char, &str, &str, bool)> = [
        ('y', "allow once", "once", true),
        ('a', "session", "session", true),
        ('p', "project", "project", false),
        ('n', "deny", "deny", true),
    ]
    .into_iter()
    .filter(|c| grantable || !matches!(c.0, 'a' | 'p'))
    .collect();
    let chip_width = |label: &str| 3 + 1 + cell_width(label);
    let full: usize = chips.iter().map(|c| chip_width(c.1)).sum::<usize>() + 3 * 3;
    let short = full > u;
    let label = |c: &(char, &'static str, &'static str, bool)| if short { c.2 } else { c.1 };
    // Deny first after "allow once" when rows must wrap: it is never pushed off.
    let deny = chips.len() - 1;
    let order: Vec<usize> = if short {
        std::iter::once(0)
            .chain(std::iter::once(deny))
            .chain(1..deny)
            .collect()
    } else {
        (0..chips.len()).collect()
    };
    let mut rows: Vec<Vec<Span<'static>>> = vec![vec![]];
    let mut used = 0;
    for i in order {
        let chip = &chips[i];
        let w = chip_width(label(chip));
        let gap = if used == 0 { 0 } else { 3 };
        if used > 0 && used + gap + w > u {
            rows.push(vec![]);
            used = 0;
        }
        let row = rows.last_mut().unwrap();
        if used > 0 {
            row.push(Span::raw("   "));
            used += 3;
        }
        let (key_style, label_style) = if chip.3 {
            (
                Style::new().fg(p::GROUND).bg(p::INK),
                Style::new().fg(p::INK),
            )
        } else {
            (Style::new().fg(p::FAINT), Style::new().fg(p::FAINT))
        };
        row.push(Span::styled(format!(" {} ", chip.0), key_style));
        row.push(Span::styled(format!(" {}", label(chip)), label_style));
        used += w;
    }
    // The note (why `p` is unavailable, what else waits) is never dropped:
    // right-aligned on the first row with room, else on a row of its own.
    if !note.is_empty() {
        let fits = rows.iter().position(|row| {
            row.iter().map(|s| cell_width(&s.content)).sum::<usize>() + 2 + cell_width(note) <= u
        });
        match fits {
            Some(at) => {
                let used: usize = rows[at].iter().map(|s| cell_width(&s.content)).sum();
                rows[at].push(Span::raw(" ".repeat(u - used - cell_width(note))));
                rows[at].push(Span::styled(note.to_owned(), Style::new().fg(p::FAINT)));
            }
            None => rows.push(vec![Span::styled(
                ellipsis(note, u),
                Style::new().fg(p::FAINT),
            )]),
        }
    }
    let mut out: Vec<Line<'static>> = rows
        .into_iter()
        .map(|spans| padded(spans, width, p::BLOCK_PLUS))
        .collect();
    if !grantable {
        for (key, name) in [('a', "session"), ('p', "project")] {
            out.push(body(
                &format!(" {key}  {name}   not grantable — destructive floor"),
                width,
                p::FAINT,
                p::BLOCK_PLUS,
            ));
        }
    }
    out
}

/// The file text of a `read` output line (`     3\tcode`), `None` for the
/// tool's own trailer rows (`[6 more lines; continue with offset=5]`).
fn read_line(row: &str) -> Option<&str> {
    let (n, text) = row.split_once('\t')?;
    n.trim().parse::<usize>().ok().map(|_| text)
}

enum MetaKind {
    Hidden,
    Expanded,
    Ask,
    Folded,
    Shell,
    Empty,
}

struct CallBody {
    input: serde_json::Value,
    rows: Vec<String>,
    signs: Vec<Option<char>>,
    /// File line numbers of diff rows (0 = unknown, the field stays blank).
    numbers: Vec<usize>,
    /// Bytes the tool itself dropped from the middle of its output.
    omitted: Option<String>,
    exit: Option<i32>,
    failed: bool,
    argument: String,
    outcome: String,
    ask: bool,
    delegate: bool,
}

impl CallBody {
    fn new(row: &ToolRow) -> Self {
        let output = row.output.as_deref().unwrap_or("");
        let input: serde_json::Value = serde_json::from_str(&row.input).unwrap_or_default();
        let get = |key: &str| input.get(key).and_then(|v| v.as_str());
        let mut rows: Vec<String> = output.lines().map(str::to_owned).collect();
        let exit = (row.name == "shell")
            .then(|| {
                rows.last()
                    .and_then(|s| s.strip_prefix("[exit code: "))
                    .and_then(|s| s.strip_suffix(']'))
                    .and_then(|s| s.parse::<i32>().ok())
            })
            .flatten();
        if exit.is_some() {
            rows.pop();
        }
        // A cancelled shell's `[cancelled]` footer is the outcome, not output.
        if row.status == RowStatus::Settled(ToolStatus::Cancelled)
            && rows.last().is_some_and(|l| l == "[cancelled]")
        {
            rows.pop();
        }
        let omitted = rows.iter().find_map(|r| {
            r.strip_prefix("[… ")?
                .strip_suffix(" bytes omitted …]")?
                .parse::<usize>()
                .ok()
                .map(size)
        });
        let settled = row.status != RowStatus::Running;
        let failed = matches!(row.status, RowStatus::Settled(s) if s != ToolStatus::Ok)
            || exit.is_some_and(|n| n != 0);
        let mut argument = row.summary.clone();
        if let Some(path) = get("file_path").or_else(|| get("path")) {
            argument = path.to_owned();
        }
        // A patch arrives as `{"patch": …}` or as the freeform text itself.
        let patch = row
            .name
            .contains("patch")
            .then(|| match get("patch") {
                Some(patch) => Some(patch.to_owned()),
                None => input.is_null().then(|| row.input.clone()),
            })
            .flatten();
        if let Some(patch) = &patch {
            let files = super::diff::parse_patch(patch).files;
            if let Some(first) = files.first() {
                argument = match files.len() {
                    1 => first.path.clone(),
                    n => format!("{} +{}", first.path, n - 1),
                };
            }
        }
        if row.name == "shell"
            && let Some(cmd) = get("command")
        {
            argument = if cmd.contains('\n') {
                get("description")
                    .map(str::to_owned)
                    .unwrap_or_else(|| format!("{}…", cmd.lines().next().unwrap_or("")))
            } else {
                cmd.to_owned()
            };
        }
        if let Some(value) = get("question")
            .or_else(|| get("name"))
            .or_else(|| get("id"))
        {
            argument = value.to_owned();
        }
        let mut signs: Vec<Option<char>> = vec![];
        let workers = input.get("workers").and_then(|v| v.as_array()).cloned();
        let delegate = row.name == "delegate" && workers.is_some();
        if let Some(workers) = workers.as_ref().filter(|_| delegate) {
            argument = plural(workers.len(), "worker");
            rows.clear();
            // Names pad to the longest so the route column lines up (§4.6).
            let value = |w: &serde_json::Value, k: &str| {
                w.get(k).and_then(|v| v.as_str()).unwrap_or("—").to_owned()
            };
            let pad = workers
                .iter()
                .map(|w| cell_width(&value(w, "name")))
                .max()
                .unwrap_or(0);
            for (i, worker) in workers.iter().enumerate() {
                if i > 0 {
                    rows.push(String::new());
                }
                let name = value(worker, "name");
                rows.push(format!(
                    "{} {name}{}   {} · {}",
                    value(worker, "state"),
                    " ".repeat(pad - cell_width(&name)),
                    value(worker, "route"),
                    value(worker, "profile")
                ));
                rows.push(format!("  owns  {}", value(worker, "owns")));
                rows.push(format!("  ↳ {}", value(worker, "activity")));
            }
        }
        let options = input.get("options").and_then(|v| v.as_array()).cloned();
        let ask = row.name == "ask" && options.is_some();
        if let Some(options) = options.as_ref().filter(|_| ask) {
            argument = get("question").unwrap_or(&argument).to_owned();
            rows = options
                .iter()
                .map(|option| {
                    let title = option.get("label").and_then(|v| v.as_str()).unwrap_or("—");
                    let title = ellipsis(title, 22);
                    format!(
                        "{title}{}{}",
                        " ".repeat(22 - cell_width(&title)),
                        option
                            .get("description")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                    )
                })
                .collect();
        }
        let mut numbers = vec![];
        if !failed && settled {
            if let Some(diff) = &row.diff {
                // The rows reviewed before it ran: real line numbers and context.
                use super::diff::DiffRow;
                rows.clear();
                for r in diff {
                    let (n, sign, text) = match r {
                        DiffRow::Add { line, text } => (*line, '+', text),
                        DiffRow::Del { line, text } => (*line, '-', text),
                        DiffRow::Context { line, text } => (*line, ' ', text),
                    };
                    rows.push(text.clone());
                    signs.push(Some(sign));
                    numbers.push(n as usize);
                }
            } else if row.name == "write"
                && let Some(content) = get("content")
            {
                rows = content.lines().map(str::to_owned).collect();
                signs = vec![Some('+'); rows.len()];
            } else if row.name == "edit"
                && let (Some(old), Some(new)) = (get("old_string"), get("new_string"))
            {
                rows = old.lines().chain(new.lines()).map(str::to_owned).collect();
                signs = std::iter::repeat_n(Some('-'), old.lines().count())
                    .chain(std::iter::repeat_n(Some('+'), new.lines().count()))
                    .collect();
            }
        }
        let elapsed = row.elapsed_ms.map(super::elapsed);
        let with = |mark: &str, facts: Vec<String>| -> String {
            let facts: Vec<String> = elapsed.iter().cloned().chain(facts).collect();
            if facts.is_empty() {
                mark.to_owned()
            } else {
                format!("{mark} {}", facts.join(" · "))
            }
        };
        let added = signs.iter().filter(|s| **s == Some('+')).count();
        let removed = signs.iter().filter(|s| **s == Some('-')).count();
        let outcome = if row.status == RowStatus::Running {
            "▪▪▪".into()
        } else if row.status == RowStatus::Settled(ToolStatus::Denied)
            && output == crate::runtime::CANCEL_DENY
        {
            // Cancelled while its approval was pending: the operator stopped it.
            "✗ cancelled".into()
        } else if let RowStatus::Settled(s) = row.status
            && !matches!(s, ToolStatus::Ok | ToolStatus::Error)
        {
            format!(
                "✗ {}",
                match s {
                    ToolStatus::Denied => "denied",
                    ToolStatus::Cancelled => "cancelled",
                    ToolStatus::Unavailable => "unavailable",
                    _ => "outcome unknown",
                }
            )
        } else if failed {
            with(
                "✗",
                exit.filter(|n| *n != 0)
                    .map(|n| format!("exit {n}"))
                    .into_iter()
                    .collect(),
            )
        } else {
            match row.name.as_str() {
                "shell" => with("✓", vec![]),
                "read" => {
                    // File lines only (paging trailers are not file text), sized
                    // without the tool's line-number prefixes.
                    // Output without number prefixes is all file text.
                    let numbered = rows.iter().any(|r| read_line(r).is_some());
                    let lines: Vec<&str> = if numbered {
                        rows.iter().filter_map(|r| read_line(r)).collect()
                    } else {
                        rows.iter().map(String::as_str).collect()
                    };
                    let bytes: usize = lines.iter().map(|t| t.len() + 1).sum();
                    format!("✓ {} · {}", plural(lines.len(), "line"), size(bytes))
                }
                // Reviewed, and the file already held exactly this.
                "write" if row.diff.as_ref().is_some_and(|d| d.is_empty()) => {
                    let lines = get("content").map_or(0, |c| c.lines().count());
                    format!("✓ unchanged · {}", plural(lines, "line"))
                }
                "write" if row.diff.is_some() => {
                    let lines = get("content").map_or(added, |c| c.lines().count());
                    if removed == 0 && !signs.contains(&Some(' ')) {
                        format!("✓ {} new", plural(lines, "line"))
                    } else {
                        format!("✓ {} replaced · +{added} −{removed}", plural(lines, "line"))
                    }
                }
                "write" if !signs.is_empty() => {
                    format!("✓ {} written", plural(rows.len(), "line"))
                }
                "patch" | "apply_patch" if !signs.is_empty() => {
                    let files = patch
                        .as_deref()
                        .map_or(1, |p| super::diff::parse_patch(p).files.len().max(1));
                    format!("+{added} −{removed} · {}", plural(files, "file"))
                }
                "edit" if !signs.is_empty() => format!("+{added} −{removed} · 1 of 1 file"),
                "skill" => "✓ loaded".into(),
                "search" => format!("✓ {}", plural(rows.len(), "hit")),
                "delegate" if delegate => format!(
                    "ceiling {} calls",
                    input
                        .get("ceiling")
                        .and_then(|v| v.as_u64())
                        .map(|n| n.to_string())
                        .unwrap_or_else(|| "—".into())
                ),
                "ask" => if input
                    .get("multiple")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false)
                {
                    "pick any"
                } else {
                    "pick one"
                }
                .into(),
                _ => with("✓", vec![plural(rows.len(), "line")]),
            }
        };
        Self {
            input,
            rows,
            signs,
            numbers,
            omitted,
            exit,
            failed,
            argument,
            outcome,
            ask,
            delegate,
        }
    }

    /// Rows a folded preview keeps; `None` when the whole body fits.
    fn fold_keep(&self, row: &ToolRow) -> Option<usize> {
        (self.rows.len() > FOLD_AT).then_some(if row.name == "write" {
            WRITE_KEEP
        } else {
            FOLD_KEEP
        })
    }

    /// Body rows shown for a disclosure state: `(first, count)`.
    fn shown(&self, row: &ToolRow, disclosure: Option<bool>) -> (usize, usize) {
        match disclosure {
            Some(false) => (0, 0),
            Some(true) => (0, self.rows.len()),
            None => match self.fold_keep(row) {
                Some(keep) if row.name == "shell" => (self.rows.len() - keep, keep),
                Some(keep) => (0, keep),
                None => (0, self.rows.len()),
            },
        }
    }

    /// Band C for this state, decided once for measuring and painting alike.
    fn meta_kind(&self, row: &ToolRow, disclosure: Option<bool>) -> Option<MetaKind> {
        let settled = row.status != RowStatus::Running;
        let folds = self.fold_keep(row).is_some();
        match disclosure {
            Some(false) => return Some(MetaKind::Hidden),
            Some(true) if folds => return Some(MetaKind::Expanded),
            _ => {}
        }
        if self.ask {
            Some(MetaKind::Ask)
        } else if disclosure.is_none() && folds {
            Some(MetaKind::Folded)
        } else if row.name == "shell"
            && settled
            && row.started
            && (self.exit.is_some() || row.status == RowStatus::Settled(ToolStatus::Cancelled))
        {
            Some(MetaKind::Shell)
        } else if self.rows.is_empty() && settled {
            Some(MetaKind::Empty)
        } else {
            None
        }
    }

    fn height(&self, row: &ToolRow, disclosure: Option<bool>) -> usize {
        1 + self.shown(row, disclosure).1 + usize::from(self.meta_kind(row, disclosure).is_some())
    }
}

/// A shell footer line (`[exit code: N]`, `[cancelled]`): the block states
/// the outcome, so the pane, a copy and a line count leave it out.
pub fn is_trailer(line: &str) -> bool {
    (line.starts_with("[exit code: ") && line.ends_with(']')) || line == "[cancelled]"
}

/// Whether a settled call failed (an error status or a non-zero exit).
pub fn call_failed(row: &ToolRow) -> bool {
    CallBody::new(row).failed
}

/// The header's argument for a call (the command, the path, …), decoded.
pub fn call_argument(row: &ToolRow) -> String {
    CallBody::new(row).argument
}

/// Whether clicking this row's header would fold it back to a preview (the body
/// is longer than the preview keeps) — otherwise a click hides the body.
pub fn foldable(row: &ToolRow) -> bool {
    CallBody::new(row).fold_keep(row).is_some()
}

/// Whether a call's output is not all on screen: folded to a preview, or a
/// row cut at the right edge of a `width`-cell transcript (`^O` then opens it).
pub fn hides_content(row: &ToolRow, width: usize) -> bool {
    foldable(row)
        || row
            .output
            .as_deref()
            .is_some_and(|o| o.lines().any(|l| cell_width(&clean(l)) + 8 > width))
}

/// The next disclosure for a header click or keyboard toggle: a folded preview
/// expands and returns; a body that fits collapses and returns. Every toggle
/// changes what is shown.
pub fn next_disclosure(row: &ToolRow, current: Option<bool>) -> Option<bool> {
    match (current, foldable(row)) {
        (None, true) => Some(true),
        (None, false) => Some(false),
        _ => None,
    }
}

pub fn call_lines(row: &ToolRow, width: usize, now: u64, reduced: bool) -> Vec<Line<'static>> {
    call_lines_disclosed(row, width, now, reduced, None)
}

fn call_lines_disclosed(
    row: &ToolRow,
    width: usize,
    now: u64,
    reduced: bool,
    disclosure: Option<bool>,
) -> Vec<Line<'static>> {
    let call = CallBody::new(row);
    let mut head = header(&row.name, &call.argument, &call.outcome, width, false);
    if disclosure == Some(true) {
        for span in &mut head.spans {
            if span.content == "▸ " {
                span.content = "▾ ".into();
            }
        }
    }
    if row.status == RowStatus::Running {
        let at = head.spans.iter().position(|s| s.content == "▪▪▪");
        if let Some(at) = at {
            head.spans
                .splice(at..at + 1, leds(now, reduced, p::BLOCK_PLUS));
        }
    }
    let mut out = vec![head];
    let (first, count) = call.shown(row, disclosure);
    let get = |key: &str| call.input.get(key).and_then(|v| v.as_str());
    // One number width for the whole block: the largest line number a read
    // shows (a window 995–1004 aligns its text), else the row count.
    let digits = call
        .rows
        .iter()
        .filter_map(|r| r.split_once('\t'))
        .filter_map(|(n, _)| n.trim().parse::<usize>().ok())
        .max()
        .unwrap_or(call.rows.len())
        .to_string()
        .len()
        .max(3);
    for (i, text) in call.rows.iter().enumerate().skip(first).take(count) {
        if call.ask {
            let focused = call
                .input
                .get("focused")
                .and_then(|v| v.as_u64())
                .unwrap_or(0) as usize
                == i;
            let u = width.saturating_sub(4);
            let line = if focused {
                // The one inverted row: the focused choice, filled across U.
                let mut spans = clipped(&format!("▸ {text}"), u, p::GROUND);
                let used: usize = spans.iter().map(|s| cell_width(&s.content)).sum();
                spans.push(Span::raw(" ".repeat(u.saturating_sub(used))));
                for span in &mut spans {
                    span.style = span.style.fg(p::GROUND).bg(p::INK);
                }
                padded(spans, width, p::BLOCK)
            } else {
                let mut spans = vec![Span::styled("· ", Style::new().fg(p::FAINT))];
                spans.extend(clipped(text, u.saturating_sub(2), p::DIM));
                padded(spans, width, p::BLOCK)
            };
            out.push(line);
        } else if call.delegate && i % 4 == 0 {
            let worker = call
                .input
                .get("workers")
                .and_then(|v| v.as_array())
                .and_then(|w| w.get(i / 4));
            let cost = worker
                .and_then(|w| w.get("cost_micro_usd"))
                .and_then(|v| v.as_u64())
                .map(|n| format!("${:.2}", n as f64 / 1_000_000.0))
                .unwrap_or_else(|| "—".into());
            let elapsed = worker
                .and_then(|w| w.get("elapsed_ms"))
                .and_then(|v| v.as_u64())
                .map(super::elapsed)
                .unwrap_or_else(|| "—".into());
            let right = format!("{elapsed} · {cost}");
            let u = width.saturating_sub(4);
            let left = ellipsis(text, u.saturating_sub(cell_width(&right) + 2));
            let gap = u.saturating_sub(cell_width(&left) + cell_width(&right));
            // Name at INK, `route · profile` DIM (§4.6).
            let (name, route) = match left.find("   ") {
                Some(at) => (left[..at].to_owned(), left[at..].to_owned()),
                None => (left, String::new()),
            };
            out.push(padded(
                vec![
                    Span::styled(name, Style::new().fg(p::INK)),
                    Span::styled(route, Style::new().fg(p::DIM)),
                    Span::raw(" ".repeat(gap)),
                    Span::styled(right, Style::new().fg(p::DIM)),
                ],
                width,
                p::BLOCK,
            ));
        } else if row.name == "read" && !call.failed {
            let numbered_raw = text
                .split_once('\t')
                .and_then(|(n, t)| n.trim().parse::<usize>().ok().map(|n| (n, t)))
                .or_else(|| {
                    (!call.rows.iter().any(|r| read_line(r).is_some()))
                        .then_some((i + 1, text.as_str()))
                });
            match numbered_raw {
                Some((n, text)) => out.push(numbered(
                    n,
                    digits.max(n.to_string().len()),
                    None,
                    text,
                    width,
                )),
                // The tool's own trailer (`[6 more lines; continue with …]`).
                None => out.push(body(text, width, p::DIM, p::BLOCK)),
            }
        } else if let Some(sign) = call.signs.get(i).copied().flatten() {
            // Real numbers when the change was reviewed before it ran; a
            // settled edit without them leaves the field blank rather than
            // claiming positions the tool never reported.
            let n = match call.numbers.get(i) {
                Some(n) => *n,
                None if row.name == "write" => i + 1,
                None => 0,
            };
            let digits = call
                .numbers
                .iter()
                .max()
                .map_or(digits, |m| digits.max(m.to_string().len()));
            out.push(numbered(n, digits, Some(sign), text, width));
        } else if matches!(row.name.as_str(), "notify" | "compact") {
            if let Some((key, value)) = text.split_once("  ") {
                out.push(label(key.trim(), value.trim_start(), width));
            } else {
                out.push(body(text, width, p::DIM, p::BLOCK));
            }
        } else {
            out.push(body(text, width, p::DIM, p::BLOCK));
        }
    }
    let Some(kind) = call.meta_kind(row, disclosure) else {
        return out;
    };
    let id = row
        .output_id
        .clone()
        .unwrap_or_else(|| FoldId::of(row.output.as_deref().unwrap_or("")));
    out.push(match kind {
        MetaKind::Hidden => meta(
            &format!("· {} hidden", plural(call.rows.len(), "line")),
            "click header to show",
            width,
        ),
        MetaKind::Expanded => fold_meta(call.rows.len(), &id, true, width, None),
        MetaKind::Ask => meta_styled(
            get("consequence").unwrap_or(""),
            p::DIM,
            "space toggle   ⏎ confirm   esc dismiss",
            width,
        ),
        MetaKind::Folded => fold_meta(
            call.rows.len() - count,
            &id,
            false,
            width,
            call.omitted.as_deref(),
        ),
        MetaKind::Shell => {
            let mut facts = vec![match call.exit {
                Some(exit) => format!("exit {exit}"),
                None => "cancelled".into(),
            }];
            facts.push(plural(call.rows.len(), "line"));
            if let Some(cwd) = get("cwd") {
                facts.push(format!("cwd {cwd}"));
            }
            body(&facts.join(" · "), width, p::DIM, p::BLOCK)
        }
        MetaKind::Empty => body(
            if call.failed {
                "No output returned"
            } else {
                "No output"
            },
            width,
            p::DIM,
            p::BLOCK,
        ),
    });
    out
}

/// The working LEDs over `bg`: opacity 0.18 → 1.0 of INK blended over the row's
/// own ground, so a resting cell never vanishes into the band (§7).
fn leds(now: u64, reduced: bool, bg: Color) -> Vec<Span<'static>> {
    let base = match bg {
        Color::Rgb(r, _, _) => f32::from(r),
        _ => 0.0,
    };
    (0..3)
        .map(|n| {
            let fg = if reduced {
                p::INK
            } else {
                let level = base + (232.0 - base) * crate::glyphs::working_opacity(n, now);
                let level = level as u8;
                Color::Rgb(level, level, level)
            };
            Span::styled("▪", Style::new().fg(fg).bg(bg))
        })
        .collect()
}

/// The working row for everything that is not a running tool: `▪▪▪ label` on
/// GROUND at column 3 (screens.html 2b). Chrome is earned by tool events only.
pub fn working_line(label: &str, width: usize, now: u64, reduced: bool) -> Line<'static> {
    let mut spans = vec![Span::raw(" ".repeat(width.min(2)))];
    spans.extend(leds(now, reduced, p::GROUND));
    if !label.is_empty() {
        spans.push(Span::styled(
            format!(" {}", ellipsis(label, width.saturating_sub(8))),
            Style::new().fg(p::DIM),
        ));
    }
    band(spans, width, p::GROUND)
}

/// A list item or quote marker at the start of a prose line: the hanging
/// indent its continuation rows take.
fn list_hang(line: &str) -> Option<usize> {
    let indent = line.len() - line.trim_start_matches(' ').len();
    let rest = &line[indent..];
    let marker = if let Some(r) = rest.strip_prefix(['-', '*', '+', '>']) {
        (r.starts_with(' ')).then_some(2)
    } else {
        let digits = rest.chars().take_while(char::is_ascii_digit).count();
        (digits > 0 && digits < 4)
            .then(|| rest[digits..].strip_prefix(['.', ')']))
            .flatten()
            .filter(|r| r.starts_with(' '))
            .map(|_| digits + 2)
    }?;
    let after = &rest[marker..];
    Some(indent + marker + (after.len() - after.trim_start_matches(' ').len()))
}

enum TextRow {
    Prose(String),
    /// Fenced code keeps its lines: clipped like a body row, never re-flowed.
    Code(String),
    Fence(String),
    Blank,
}

/// Whether a prose line starts its own markdown block rather than continuing
/// the paragraph above: headings, list items, quotes, tables, indented code.
fn starts_block(line: &str) -> bool {
    let t = line.trim_start();
    t.starts_with('#')
        || t.starts_with('|')
        || list_hang(line).is_some()
        || line.starts_with("    ")
}

/// Prose rows at `width`: tabs and controls cleaned BEFORE wrapping (so widths
/// are real), whitespace kept, list items hang, fences never wrap, runs of blank
/// lines collapse and the block never starts or ends blank. A paragraph's source
/// line breaks are markdown soft breaks: its lines join and re-wrap as one (a
/// line ending in two spaces or `\` keeps its break).
fn prose_rows(lines: &[String], width: usize) -> Vec<TextRow> {
    let u = width.saturating_sub(4);
    let mut out = vec![];
    let mut fence = false;
    // The paragraph being joined, flushed on any block boundary.
    let mut para: Option<String> = None;
    let flush = |para: &mut Option<String>, out: &mut Vec<TextRow>| {
        if let Some(text) = para.take() {
            let hang = list_hang(&text).unwrap_or(text.len() - text.trim_start().len());
            out.extend(
                crate::wrap::wrap_hanging(&text, u, hang)
                    .into_iter()
                    .map(TextRow::Prose),
            );
        }
    };
    for raw in lines {
        let line = clean(raw);
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            flush(&mut para, &mut out);
            fence = !fence;
            out.push(TextRow::Fence(line));
            continue;
        }
        if fence {
            out.push(TextRow::Code(line));
            continue;
        }
        if line.trim().is_empty() {
            flush(&mut para, &mut out);
            if !matches!(out.last(), None | Some(TextRow::Blank)) {
                out.push(TextRow::Blank);
            }
            continue;
        }
        match &mut para {
            Some(text)
                if !starts_block(&line)
                    && !text.ends_with("  ")
                    && !text.ends_with('\\')
                    && !text.trim_start().starts_with('#')
                    && !text.trim_start().starts_with('|') =>
            {
                text.push(' ');
                text.push_str(trimmed);
            }
            _ => {
                flush(&mut para, &mut out);
                para = Some(line);
            }
        }
    }
    flush(&mut para, &mut out);
    // Never end on blank rows — nor on the empty tail of an unclosed fence.
    while matches!(out.last(), Some(TextRow::Blank))
        || matches!(out.last(), Some(TextRow::Code(t)) if t.trim().is_empty())
    {
        out.pop();
    }
    out
}

/// Operator input longer than this folds to its head (a pasted log must not
/// push the conversation off the screen); a click on the fold row shows it all.
const OPERATOR_FOLD_AT: usize = 12;
const OPERATOR_KEEP: usize = 8;

/// Rows of a non-tool event, as (text, colour) plus the band it sits on.
fn text_rows(block: &Block, width: usize, disclosure: Option<bool>) -> Vec<(String, Color)> {
    let mut rows = vec![];
    match block {
        Block::Call(_) => {}
        Block::Operator { text } => {
            // A paste that ended in a newline has no blank last line to show.
            let mut line_of = vec![];
            for (i, part) in text.trim_end_matches(['\n', '\r']).split('\n').enumerate() {
                let part = clean(part);
                for (j, line) in crate::wrap::wrap(&part, width.saturating_sub(6))
                    .into_iter()
                    .enumerate()
                {
                    let prefix = if i == 0 && j == 0 { "› " } else { "  " };
                    rows.push((format!("{prefix}{line}"), p::INK));
                    line_of.push(i);
                }
            }
            if rows.len() > OPERATOR_FOLD_AT {
                if disclosure == Some(true) {
                    rows.push(("  · click to fold".into(), p::FAINT));
                } else {
                    // Counted in the prompt's own lines (as the composer counts
                    // them), not in wrapped rows.
                    let more = line_of.last().map_or(0, |last| last + 1)
                        - line_of.get(OPERATOR_KEEP).copied().unwrap_or(0);
                    rows.truncate(OPERATOR_KEEP);
                    rows.push((
                        format!(
                            "  · {more} more {} — click to show",
                            if more == 1 { "line" } else { "lines" }
                        ),
                        p::FAINT,
                    ));
                }
            }
        }
        Block::Prose { lines } => {
            for row in prose_rows(lines, width) {
                rows.push(match row {
                    TextRow::Prose(text) | TextRow::Code(text) => (text, p::INK),
                    TextRow::Fence(text) => (text, p::DIM),
                    TextRow::Blank => (String::new(), p::INK),
                });
            }
        }
        // A failure is told apart from prose by its glyph, not only by colour
        // (§10.1): `✗` then the detail, hanging under it.
        Block::Notice { lines } => {
            for (i, text) in lines.iter().enumerate() {
                let text = clean(text);
                for (j, line) in crate::wrap::wrap_hanging(&text, width.saturating_sub(6), 0)
                    .into_iter()
                    .enumerate()
                {
                    let prefix = if i == 0 && j == 0 { "✗ " } else { "  " };
                    rows.push((format!("{prefix}{line}"), p::DIM));
                }
            }
        }
        // Verbatim host text: exact spacing, clipped, never wrapped.
        Block::Info { lines } => {
            for text in lines {
                rows.push((text.clone(), p::DIM));
            }
        }
        Block::Meta { text } => {
            let text = clean(text);
            let hang = if text.starts_with("· ") || text.starts_with("↳ ") {
                2
            } else {
                0
            };
            for line in crate::wrap::wrap_hanging(&text, width.saturating_sub(4), hang) {
                rows.push((line, p::DIM));
            }
        }
        Block::Reasoning {
            lines,
            expanded,
            elapsed_ms,
        } => {
            let elapsed = elapsed_ms.map(super::elapsed);
            rows.push((
                format!(
                    "· reasoning{}   ^R {}",
                    elapsed.map(|e| format!(" {e}")).unwrap_or_default(),
                    if *expanded { "collapse" } else { "expand" }
                ),
                p::FAINT,
            ));
            if *expanded {
                for row in prose_rows(lines, width) {
                    rows.push(match row {
                        TextRow::Prose(t) | TextRow::Code(t) | TextRow::Fence(t) => (t, p::DIM),
                        TextRow::Blank => (String::new(), p::DIM),
                    });
                }
            }
        }
    }
    rows
}

fn text_lines(block: &Block, width: usize, disclosure: Option<bool>) -> Vec<Line<'static>> {
    text_rows(block, width, disclosure)
        .into_iter()
        .map(|(text, fg)| {
            if text.is_empty() {
                band(vec![], width, p::GROUND)
            } else {
                body(&text, width, fg, p::GROUND)
            }
        })
        .collect()
}

fn block_height(block: &Block, width: usize, disclosure: Option<bool>) -> usize {
    match block {
        Block::Call(row) => CallBody::new(row).height(row, disclosure),
        _ => text_rows(block, width, disclosure).len(),
    }
}

fn block_lines(
    block: &Block,
    width: usize,
    now: u64,
    reduced: bool,
    disclosure: Option<bool>,
) -> Vec<Line<'static>> {
    match block {
        Block::Call(row) => call_lines_disclosed(row, width, now, reduced, disclosure),
        _ => text_lines(block, width, disclosure),
    }
}

/// The whole transcript, unclipped: every block, one blank row between events,
/// then the working row. Tests and search use it; frames use [`viewport`].
pub fn lines(
    transcript: &Transcript,
    width: usize,
    working: Option<&str>,
    now: u64,
    reduced: bool,
) -> Vec<Line<'static>> {
    let mut out = vec![];
    for (index, block) in transcript.blocks.iter().enumerate() {
        let event = block_lines(
            block,
            width,
            now,
            reduced,
            transcript.disclosures.get(&index).copied(),
        );
        if !out.is_empty() && !event.is_empty() {
            out.push(Line::default());
        }
        out.extend(event);
    }
    if let Some(label) = working
        && transcript.running_indices().next().is_none()
    {
        if !out.is_empty() {
            out.push(Line::default());
        }
        out.push(working_line(label, width, now, reduced));
    }
    out
}

/// Per-block layout at one width. `own[i]` is block i's row count without its
/// separator; a block gets one blank separator row above it when it has rows
/// and some earlier block does. Styled rows exist only for blocks a frame drew.
#[derive(Debug, Default)]
pub struct Cache {
    width: usize,
    own: Vec<usize>,
    /// Row of block i's first content row (after its separator).
    starts: Vec<usize>,
    /// Row of block i's first row, separator included.
    begins: Vec<usize>,
    separated: Vec<bool>,
    total: usize,
    stale: Vec<bool>,
    rows: Vec<Option<Vec<Line<'static>>>>,
    /// Lowest index whose `starts` need recomputing.
    dirty_from: Option<usize>,
    /// Styled blocks rendered so far (tests assert settled blocks are reused).
    pub rendered_events: usize,
}

impl Cache {
    pub fn invalidate(&mut self, index: usize) {
        if let Some(stale) = self.stale.get_mut(index) {
            *stale = true;
        }
        if let Some(rows) = self.rows.get_mut(index) {
            *rows = None;
        }
        self.dirty_from = Some(self.dirty_from.map_or(index, |n| n.min(index)));
    }

    fn update(&mut self, transcript: &Transcript, width: usize) {
        let blocks = &transcript.blocks;
        if self.own.len() > blocks.len() {
            *self = Self {
                rendered_events: self.rendered_events,
                ..Self::default()
            };
        }
        if self.width != width {
            self.width = width;
            // Tool bodies never wrap: their heights survive a width change.
            for (i, block) in blocks.iter().enumerate().take(self.own.len()) {
                if !matches!(block, Block::Call(_)) {
                    self.stale[i] = true;
                }
            }
            self.rows.iter_mut().for_each(|r| *r = None);
            self.dirty_from = Some(0);
        }
        if self.own.len() < blocks.len() {
            let from = self.own.len();
            self.own.resize(blocks.len(), 0);
            self.stale.resize(blocks.len(), true);
            self.rows.resize_with(blocks.len(), || None);
            self.dirty_from = Some(self.dirty_from.map_or(from, |n| n.min(from)));
        }
        let Some(from) = self.dirty_from.take() else {
            return;
        };
        for (i, block) in blocks.iter().enumerate().skip(from) {
            if self.stale[i] {
                self.own[i] = block_height(block, width, transcript.disclosures.get(&i).copied());
                self.stale[i] = false;
            }
        }
        self.starts.resize(blocks.len(), 0);
        self.begins.resize(blocks.len(), 0);
        self.separated.resize(blocks.len(), false);
        let (mut row, mut any) = if from == 0 {
            (0, false)
        } else {
            (
                self.starts[from - 1] + self.own[from - 1],
                self.starts[from - 1] + self.own[from - 1] > 0,
            )
        };
        for i in from..blocks.len() {
            let sep = any && self.own[i] > 0;
            self.separated[i] = sep;
            self.begins[i] = row;
            self.starts[i] = row + usize::from(sep);
            row = self.starts[i] + self.own[i];
            any |= self.own[i] > 0;
        }
        self.total = row;
    }

    /// The styled rows of block `i`, rendered once per width.
    fn block_rows(&mut self, transcript: &Transcript, i: usize) -> &[Line<'static>] {
        if self.rows[i].is_none() {
            let lines = block_lines(
                &transcript.blocks[i],
                self.width,
                0,
                true,
                transcript.disclosures.get(&i).copied(),
            );
            debug_assert_eq!(lines.len(), self.own[i], "measured height of block {i}");
            self.rendered_events += 1;
            self.rows[i] = Some(lines);
        }
        self.rows[i].as_deref().unwrap_or_default()
    }

    /// The block whose extent (separator included) holds `row`, and the offset
    /// of `row` from that block's first row.
    fn locate(&self, row: usize) -> Option<(usize, usize)> {
        if self.begins.is_empty() || row >= self.total {
            return None;
        }
        // Empty blocks share a begin with the next block; the last one wins,
        // which is the block that actually owns rows there.
        let i = self
            .begins
            .partition_point(|&begin| begin <= row)
            .saturating_sub(1);
        Some((i, row - self.begins[i]))
    }

    /// First row (separator included) of block `index`, plus `offset` clamped to
    /// the block's extent.
    fn row_of(&self, index: usize, offset: usize) -> Option<usize> {
        let begin = *self.begins.get(index)?;
        let extent = self.starts[index] + self.own[index] - begin;
        Some(begin + offset.min(extent.saturating_sub(1)))
    }
}

/// Bring the layout up to date at `width` and return the transcript's row count
/// (tail rows excluded).
pub fn measure(transcript: &Transcript, width: usize) -> usize {
    let mut cache = transcript.render_cache.borrow_mut();
    cache.update(transcript, width);
    cache.total
}

/// `(block, offset)` of transcript row `row` at the last measured width.
pub fn anchor_of(transcript: &Transcript, row: usize) -> Option<(usize, usize)> {
    transcript.render_cache.borrow().locate(row)
}

/// The row an anchor resolves to at the last measured width.
pub fn row_of_anchor(transcript: &Transcript, anchor: (usize, usize)) -> Option<usize> {
    transcript.render_cache.borrow().row_of(anchor.0, anchor.1)
}

/// Rows block `index` occupies, separator included.
pub fn block_extent(transcript: &Transcript, index: usize) -> Option<usize> {
    let cache = transcript.render_cache.borrow();
    let begin = *cache.begins.get(index)?;
    Some(cache.starts[index] + cache.own[index] - begin)
}

/// Whether block `index` has a blank separator row above it.
pub fn separated(transcript: &Transcript, index: usize) -> bool {
    transcript
        .render_cache
        .borrow()
        .separated
        .get(index)
        .copied()
        .unwrap_or(false)
}

/// The first painted row of block `index` (a tool's header) at the last width.
pub fn block_header(transcript: &Transcript, index: usize) -> Option<Line<'static>> {
    let mut cache = transcript.render_cache.borrow_mut();
    if index >= cache.rows.len() {
        return None;
    }
    cache.block_rows(transcript, index).first().cloned()
}

/// First content row of block `index` (after its separator).
/// The rows block `index` would occupy at `width`, separator included (as
/// [`block_extent`] counts them; the cache holds only the current width):
/// what a re-wrap is scaled against.
pub fn extent_at(transcript: &Transcript, index: usize, width: usize) -> usize {
    transcript.blocks.get(index).map_or(0, |block| {
        block_height(block, width, transcript.disclosures.get(&index).copied())
            + usize::from(separated(transcript, index))
    })
}

pub fn block_start(transcript: &Transcript, index: usize) -> Option<usize> {
    transcript.render_cache.borrow().starts.get(index).copied()
}

pub fn viewport(
    transcript: &Transcript,
    width: usize,
    height: usize,
    top: Option<usize>,
    working: Option<&str>,
    now: u64,
    reduced: bool,
) -> (usize, Vec<Line<'static>>) {
    let tail = match working {
        Some(label) if transcript.running_indices().next().is_none() => {
            vec![working_line(label, width, now, reduced)]
        }
        _ => vec![],
    };
    viewport_with(transcript, width, height, top, &tail, now, reduced)
}

/// The visible slice of the transcript plus `tail` (rows appended after the last
/// event, e.g. the working row or a pending approval), laid out lazily.
pub fn viewport_with(
    transcript: &Transcript,
    width: usize,
    height: usize,
    top: Option<usize>,
    tail: &[Line<'static>],
    now: u64,
    reduced: bool,
) -> (usize, Vec<Line<'static>>) {
    let mut cache = transcript.render_cache.borrow_mut();
    cache.update(transcript, width);
    let body = cache.total;
    let tail_start = body + usize::from(body > 0 && !tail.is_empty());
    let total = if tail.is_empty() {
        body
    } else {
        tail_start + tail.len()
    };
    let start = top
        .unwrap_or_else(|| total.saturating_sub(height))
        .min(total.saturating_sub(height));
    let end = (start + height).min(total);
    let mut lines = Vec::with_capacity(end - start);
    if start < body {
        let (first, _) = cache.locate(start).unwrap_or((0, 0));
        let mut row = start;
        let mut i = first;
        while row < end.min(body) && i < transcript.blocks.len() {
            let content = cache.starts[i];
            if cache.separated[i] && row + 1 == content {
                lines.push(Line::default());
                row += 1;
            }
            let own = cache.own[i];
            if own > 0 && row < end.min(body) && row >= content && row < content + own {
                let skip = row - content;
                let take = (content + own - row).min(end - row);
                let running = matches!(&transcript.blocks[i], Block::Call(r) if r.status == RowStatus::Running);
                let block_rows = cache.block_rows(transcript, i);
                lines.extend_from_slice(&block_rows[skip..skip + take]);
                if running
                    && skip == 0
                    && let Block::Call(r) = &transcript.blocks[i]
                {
                    // Only the running header animates; everything else is cached.
                    let at = lines.len() - take;
                    lines[at] = call_lines_disclosed(
                        r,
                        width,
                        now,
                        reduced,
                        transcript.disclosures.get(&i).copied(),
                    )
                    .swap_remove(0);
                }
                row += take;
            }
            i += 1;
        }
    }
    for row in start.max(body)..end {
        if row < tail_start {
            lines.push(Line::default());
        } else {
            lines.push(tail[row - tail_start].clone());
        }
    }
    (total, lines)
}

fn plain_text(line: &Line<'_>) -> String {
    line.spans.iter().map(|s| s.content.as_ref()).collect()
}

/// Whether a block's own text (not only what it shows) holds `needle`.
fn source_contains(block: &Block, needle: &str) -> bool {
    let has = |t: &str| t.to_lowercase().contains(needle);
    match block {
        Block::Operator { text } | Block::Meta { text } => has(text),
        // Soft breaks are joined when painted, so a phrase may span them.
        Block::Prose { lines } | Block::Notice { lines } | Block::Info { lines } => {
            has(&lines.join(" ")) || lines.iter().any(|l| has(l))
        }
        Block::Reasoning { lines, .. } => {
            has("reasoning") || has(&lines.join(" ")) || lines.iter().any(|l| has(l))
        }
        Block::Call(row) => {
            has(&row.name)
                || has(&row.summary)
                || call_input_contains(row, needle)
                || row.output.as_deref().is_some_and(has)
        }
    }
}

/// Whether the call's input VALUES hold `needle` (lowercase): the JSON keys
/// and quoting are the wire format, not what the operator reads.
pub fn call_input_contains(row: &ToolRow, needle: &str) -> bool {
    fn walk(value: &serde_json::Value, needle: &str) -> bool {
        match value {
            serde_json::Value::String(s) => s.to_lowercase().contains(needle),
            serde_json::Value::Array(items) => items.iter().any(|v| walk(v, needle)),
            serde_json::Value::Object(map) => map.values().any(|v| walk(v, needle)),
            other => other.to_string().contains(needle),
        }
    }
    match serde_json::from_str::<serde_json::Value>(&row.input) {
        Ok(value) => walk(&value, needle),
        Err(_) => row.input.to_lowercase().contains(needle),
    }
}

/// One `/find` hit: the block, which match within it (`ordinal`), the row it
/// is on, and whether a row shows it (`false`: in a folded body, collapsed
/// reasoning or the call's input). `layout` is the block's layout the row was
/// counted at — a re-wrap or a toggle re-resolves it (`resolve_hit`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SearchHit {
    pub block: usize,
    pub ordinal: usize,
    pub offset: usize,
    pub visible: bool,
    pub layout: (usize, Option<bool>, bool),
}

/// What decides a block's rows at `width`: the width and its disclosure
/// (and, for reasoning, whether it is expanded).
pub fn layout_key(
    transcript: &Transcript,
    block: usize,
    width: usize,
) -> (usize, Option<bool>, bool) {
    let expanded = matches!(
        transcript.blocks.get(block),
        Some(Block::Reasoning { expanded: true, .. })
    );
    (width, transcript.disclosures.get(&block).copied(), expanded)
}

/// The rows of one block that show `needle` (lowercase), in order. A phrase
/// split by a soft wrap counts on the row where it starts.
fn block_matches(transcript: &Transcript, index: usize, width: usize, needle: &str) -> Vec<usize> {
    let Some(block) = transcript.blocks.get(index) else {
        return vec![];
    };
    let lines = block_lines(
        block,
        width,
        0,
        true,
        transcript.disclosures.get(&index).copied(),
    );
    let texts: Vec<String> = lines.iter().map(|l| plain_text(l).to_lowercase()).collect();
    // The rows joined as they read (a wrap is a space): a phrase over any
    // number of rows is found and counted on the row where it starts.
    let mut joined = String::new();
    let mut starts = vec![];
    for text in &texts {
        starts.push(joined.len());
        joined.push_str(text.trim());
        joined.push(' ');
    }
    let mut out: Vec<usize> = vec![];
    let mut from = 0;
    while !needle.is_empty()
        && let Some(at) = joined[from..].find(needle)
    {
        let at = from + at;
        let row = starts.partition_point(|&s| s <= at).saturating_sub(1);
        if out.last() != Some(&row) {
            out.push(row);
        }
        from = at + needle.len();
    }
    // A word hard-broken at the edge (no space at the break): rows joined
    // tight find it.
    if out.is_empty() {
        let mut tight = String::new();
        let mut starts = vec![];
        for text in &texts {
            starts.push(tight.len());
            tight.push_str(text.trim());
        }
        if let Some(at) = tight.find(needle) {
            out.push(starts.partition_point(|&s| s <= at).saturating_sub(1));
        }
    }
    // Text in a row cut at the right edge (a code line): its row is the hit.
    if out.is_empty()
        && let Block::Prose { lines } | Block::Notice { lines } = block
    {
        for (n, text) in texts.iter().enumerate() {
            let shown = text.trim().trim_end_matches('›').trim_end();
            if text.trim_end().ends_with('›')
                && !shown.is_empty()
                && lines.iter().any(|l| {
                    let l = l.to_lowercase();
                    l.contains(needle) && l.trim_start().starts_with(shown)
                })
            {
                out.push(n);
            }
        }
    }
    out
}

/// The current rows of one block that hold `needle`, as hits.
pub fn find_hits_in(
    transcript: &Transcript,
    width: usize,
    needle: &str,
    block: usize,
) -> Vec<SearchHit> {
    hits_of(transcript, block, width, &needle.to_lowercase())
}

/// Every hit for `needle` (case-insensitive), oldest first. Only blocks whose
/// text holds it are rendered.
pub fn find_hits(transcript: &Transcript, width: usize, needle: &str) -> Vec<SearchHit> {
    find_hits_from(transcript, width, needle, 0)
}

/// The hits in blocks `from..` only.
pub fn find_hits_from(
    transcript: &Transcript,
    width: usize,
    needle: &str,
    from: usize,
) -> Vec<SearchHit> {
    let needle = needle.to_lowercase();
    (from..transcript.blocks.len())
        .flat_map(|i| hits_of(transcript, i, width, &needle))
        .collect()
}

/// One block's hits for a lowercase `needle`: one per row showing it, or one
/// hidden hit when only its source holds it.
fn hits_of(transcript: &Transcript, i: usize, width: usize, needle: &str) -> Vec<SearchHit> {
    let Some(block) = transcript.blocks.get(i) else {
        return vec![];
    };
    if needle.is_empty() || !source_contains(block, needle) {
        return vec![];
    }
    let layout = layout_key(transcript, i, width);
    let rows = block_matches(transcript, i, width, needle);
    if rows.is_empty() {
        return vec![SearchHit {
            block: i,
            ordinal: 0,
            offset: 0,
            visible: false,
            layout,
        }];
    }
    rows.into_iter()
        .enumerate()
        .map(|(ordinal, offset)| SearchHit {
            block: i,
            ordinal,
            offset,
            visible: true,
            layout,
        })
        .collect()
}

/// Re-count a hit's row after its block changed layout (a resize re-wrapped
/// it, a click expanded or folded it): the same match, found again. A match
/// no row shows any more is not painted.
pub fn resolve_hit(transcript: &Transcript, hit: &mut SearchHit, width: usize, needle: &str) {
    let layout = layout_key(transcript, hit.block, width);
    if hit.layout == layout {
        return;
    }
    hit.layout = layout;
    let rows = block_matches(transcript, hit.block, width, &needle.to_lowercase());
    match rows.get(hit.ordinal).or(rows.last()) {
        Some(&offset) => {
            hit.offset = offset;
            hit.visible = true;
        }
        None => {
            hit.offset = 0;
            hit.visible = false;
        }
    }
}

/// Rows of the whole transcript whose text contains `needle` (case-insensitive),
/// oldest first.
pub fn find_all(transcript: &Transcript, width: usize, needle: &str) -> Vec<usize> {
    let hits = find_hits(transcript, width, needle);
    let mut cache = transcript.render_cache.borrow_mut();
    cache.update(transcript, width);
    hits.into_iter()
        .filter(|h| h.visible)
        .map(|h| cache.starts[h.block] + h.offset)
        .collect()
}

pub fn find(transcript: &Transcript, width: usize, needle: &str) -> Option<usize> {
    find_all(transcript, width, needle).into_iter().next()
}

/// What a click on a transcript row can act on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Hit {
    /// A tool header: toggles that block's disclosure.
    Header(usize),
    /// A reasoning row: expands or collapses that reasoning block.
    Reasoning(usize),
    /// A fold row: opens that output in the pane.
    Fold(FoldId),
    /// A long operator prompt's fold row: shows or folds the whole prompt.
    Operator(usize),
}

/// Clickable rows in `[top, top + height)`, as (row from top, hit). Geometry comes
/// from the same cached extents as painting (Iris's pager hit map).
pub fn hits(transcript: &Transcript, top: usize, height: usize) -> Vec<(usize, Hit)> {
    let cache = transcript.render_cache.borrow();
    let Some((first, _)) = cache.locate(top) else {
        return vec![];
    };
    let mut out = vec![];
    // Only blocks the cache has measured: a block appended since the last
    // frame has no geometry yet (and nothing on screen to hit).
    for i in first..transcript.blocks.len().min(cache.starts.len()) {
        let start = cache.starts[i];
        if start >= top + height {
            break;
        }
        let own = cache.own[i];
        if own == 0 {
            continue;
        }
        let visible = |row: usize| (row >= top && row < top + height).then(|| row - top);
        match &transcript.blocks[i] {
            Block::Call(row) => {
                if let Some(r) = visible(start) {
                    out.push((r, Hit::Header(i)));
                }
                let last = start + own - 1;
                if own > 1
                    && let Some(id) = &row.output_id
                    && let Some(r) = visible(last)
                    && transcript.disclosures.get(&i) != Some(&false)
                    && cache.rows[i].as_ref().is_some_and(|rows| {
                        rows.last().is_some_and(|l| {
                            l.spans
                                .iter()
                                .any(|s| s.content.contains(&format!("[{id}]")))
                        })
                    })
                {
                    out.push((r, Hit::Fold(id.clone())));
                }
            }
            Block::Reasoning { .. } => {
                if let Some(r) = visible(start) {
                    out.push((r, Hit::Reasoning(i)));
                }
            }
            Block::Operator { .. }
                if cache.rows[i].as_ref().is_some_and(|rows| {
                    rows.last().is_some_and(|l| {
                        l.spans.iter().any(|s| {
                            s.content.contains("· click to")
                                || s.content.contains("— click to show")
                        })
                    })
                }) =>
            {
                if let Some(r) = visible(start + own - 1) {
                    out.push((r, Hit::Operator(i)));
                }
            }
            _ => {}
        }
    }
    out
}

/// Hit geometry comes from the same cached row ranges as painting, like Iris's pager.
pub fn visible_headers(transcript: &Transcript, top: usize, height: usize) -> Vec<(usize, usize)> {
    hits(transcript, top, height)
        .into_iter()
        .filter_map(|(row, hit)| match hit {
            Hit::Header(i) => Some((row, i)),
            _ => None,
        })
        .collect()
}
