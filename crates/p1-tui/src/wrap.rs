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

/// The longest prefix of `s` fitting `cells` display cells (never splits a
/// char — a too-wide char simply doesn't fit).
pub fn fit_cells(s: &str, cells: usize) -> String {
    let mut out = String::new();
    let mut used = 0;
    for ch in s.chars() {
        let w = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
        if used + w > cells {
            break;
        }
        out.push(ch);
        used += w;
    }
    out
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
    for word in text.split(' ') {
        if word.is_empty() {
            continue;
        }
        let word_len = cell_width(word);
        let current_len = cell_width(&current);
        if current_len > 0 && current_len + 1 + word_len > body_width {
            lines.push(std::mem::take(&mut current));
        } else if current_len > 0 {
            current.push(' ');
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
        } else {
            current.push_str(word);
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
}
