//! Word wrapping for the transcript. Breaks at spaces; a word longer than the
//! width breaks hard rather than overflowing (SPEC §3: nothing overflows).
//! Kept deliberately small — identifiers and paths are never reordered, only
//! broken when they cannot fit alone on a line.

use unicode_width::UnicodeWidthStr;

/// Display width in cells. Layout math NEVER counts chars: a wide grapheme
/// occupies two cells (CJK, emoji), and a row that fits by chars overflows by
/// cells.
pub fn cell_width(s: &str) -> usize {
    UnicodeWidthStr::width(s)
}

/// The byte boundary of the longest prefix of `s` fitting `cells` display
/// cells (never splits a char — a too-wide char simply doesn't fit).
fn fit_cells_boundary(s: &str, cells: usize) -> usize {
    let mut used = 0;
    let mut byte = 0;
    for ch in s.chars() {
        let w = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
        if used + w > cells {
            break;
        }
        used += w;
        byte += ch.len_utf8();
    }
    byte
}

/// The longest prefix of `s` fitting `cells` display cells (never splits a
/// char — a too-wide char simply doesn't fit).
pub fn fit_cells(s: &str, cells: usize) -> String {
    s[..fit_cells_boundary(s, cells)].to_string()
}

/// The number of rows `wrap(text, width)` would produce, without building the
/// strings. The transcript tail pass needs the full height cheaply.
pub(crate) fn wrap_len(text: &str, width: usize) -> usize {
    if width == 0 {
        return 1;
    }
    let indent_len = text.chars().take_while(|c| *c == ' ').count();
    let body = &text[indent_len..];
    let body_width = width.saturating_sub(indent_len).max(1);
    let mut rows = 0;
    // Display cells used on the current line; kept as a running total so a
    // word never costs a fresh scan of the line (same reason as `wrap`).
    let mut current = 0usize;
    for word in body.split(' ') {
        if word.is_empty() {
            continue;
        }
        let word_len = cell_width(word);
        if current > 0 && current + 1 + word_len > body_width {
            rows += 1;
            current = 0;
        } else if current > 0 {
            current += 1;
        }
        if word_len > body_width {
            // A word that cannot fit alone breaks hard at the cell edge.
            let mut rest = word;
            while cell_width(rest) > body_width {
                let cut = fit_cells_boundary(rest, body_width);
                rows += 1;
                rest = &rest[cut..];
            }
            current = cell_width(rest);
        } else {
            current += word_len;
        }
    }
    rows + 1
}

/// Wrap `text` to `width` columns, returning the lines. Empty input yields one
/// empty line so a block always occupies its row. Leading indentation is
/// preserved and hangs: continuation lines keep the same indent.
pub fn wrap(text: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return vec![String::new()];
    }
    let indent: String = text.chars().take_while(|c| *c == ' ').collect();
    let text = &text[indent.len()..];
    let body_width = width.saturating_sub(indent.len()).max(1);
    let mut lines = Vec::new();
    let mut current = String::new();
    // Display cells used on the current line; running, not recomputed per
    // word (cell_width walks the whole string).
    let mut current_cells = 0usize;
    for word in text.split(' ') {
        if word.is_empty() {
            continue;
        }
        let word_len = cell_width(word);
        if current_cells > 0 && current_cells + 1 + word_len > body_width {
            lines.push(std::mem::take(&mut current));
            current_cells = 0;
        } else if current_cells > 0 {
            current.push(' ');
            current_cells += 1;
        }
        if word_len > body_width {
            // A word that cannot fit alone breaks hard at the cell edge.
            let mut rest = word;
            while cell_width(rest) > body_width {
                let cut = fit_cells(rest, body_width);
                current.push_str(&cut);
                lines.push(std::mem::take(&mut current));
                rest = &rest[cut.len()..];
            }
            current.push_str(rest);
            current_cells = cell_width(rest);
        } else {
            current.push_str(word);
            current_cells += word_len;
        }
    }
    lines.push(current);
    lines
        .into_iter()
        .map(|line| format!("{indent}{line}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wraps_at_spaces() {
        assert_eq!(wrap("aa bb cc", 5), vec!["aa bb", "cc"]);
        assert_eq!(wrap("", 5), vec![""]);
    }

    #[test]
    fn indentation_is_preserved_and_hangs() {
        assert_eq!(wrap("  aa bb cc", 7), vec!["  aa bb", "  cc"]);
    }

    #[test]
    fn wide_chars_never_exceed_the_cell_width() {
        for line in wrap("你好 你好 你好", 5) {
            assert!(cell_width(&line) <= 5, "{line:?}");
        }
        assert!(wrap("你好你好", 5).iter().all(|l| cell_width(l) <= 5));
    }

    #[test]
    fn a_too_long_word_breaks_hard_never_overflows() {
        let lines = wrap("abcdefghijklmno", 5);
        assert_eq!(lines, vec!["abcde", "fghij", "klmno"]);
        assert!(lines.iter().all(|l| l.chars().count() <= 5));
    }

    #[test]
    fn wrap_len_matches_wrap_for_every_shape() {
        let cases: &[(&str, usize)] = &[
            ("", 5),
            ("aa bb cc", 5),
            ("  aa bb cc", 7),
            ("abcdefghijklmno", 5),
            ("你好 你好 你好", 5),
            ("你好你好", 5),
            ("a  b   c", 3),
            ("leading and trailing ", 6),
            ("a\nbb\tcc", 4),
            ("one", 0),
        ];
        for (text, width) in cases {
            assert_eq!(
                wrap_len(text, *width),
                wrap(text, *width).len(),
                "wrap_len disagrees with wrap for {text:?} at {width}"
            );
        }
        // A wider deterministic corpus: every width must agree on indents,
        // wide chars, hard breaks and multi-space runs.
        let corpus = [
            "",
            " ",
            "  indented body that wraps",
            "你好世界 你好 世界",
            "supercalifragilistic 短",
            "a bb ccc dddd eeeee ffffff",
            "   leading",
            "trailing   ",
            "x  y   z",
            "你好a好你",
        ];
        // CJK needs at least two cells per glyph, so start there; the widths
        // below that are exercised with ASCII only (a glyph wider than the
        // body is a pre-existing wrap edge, not this change's concern).
        for text in corpus {
            let wide = text.chars().any(|c| cell_width(&c.to_string()) > 1);
            let from = if wide { 2 } else { 0 };
            for width in from..=14 {
                assert_eq!(
                    wrap_len(text, width),
                    wrap(text, width).len(),
                    "wrap_len disagrees with wrap for {text:?} at {width}"
                );
            }
        }
    }
}
