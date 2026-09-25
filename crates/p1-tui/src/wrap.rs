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
    let indent = cell_width(&text[..text.len() - text.trim_start_matches(' ').len()]);
    wrap_hanging(text, width, indent)
}

/// Wrap at spaces, keeping runs of spaces inside a row exactly as written (only
/// the spaces a row breaks at are dropped). Continuation rows start `hang`
/// cells in; a word too long for a row breaks hard at the cell edge.
pub fn wrap_hanging(text: &str, width: usize, hang: usize) -> Vec<String> {
    if width == 0 {
        return vec![String::new()];
    }
    let hang = hang.min(width.saturating_sub(1));
    let mut lines = Vec::new();
    let mut current = String::new();
    let mut used = 0;
    // Cells a row holds before its first word (the leading indent or the hang).
    let mut floor = 0;
    let mut rest = text;
    let mut first = true;
    while !rest.is_empty() {
        let spaces_len = rest.len() - rest.trim_start_matches(' ').len();
        let (spaces, after) = rest.split_at(spaces_len);
        let word_len = after.find(' ').unwrap_or(after.len());
        let (word, after) = after.split_at(word_len);
        rest = after;
        if first && current.is_empty() {
            // Leading indentation belongs to the first row.
            let indent = fit_cells(spaces, width.saturating_sub(1));
            used = cell_width(&indent);
            floor = used;
            current.push_str(&indent);
            first = false;
        } else if !spaces.is_empty() && used > floor {
            let gap = cell_width(spaces);
            if used + gap + cell_width(word).min(1) <= width
                && (word.is_empty() || used + gap + cell_width(word) <= width)
            {
                current.push_str(spaces);
                used += gap;
            } else if !word.is_empty() {
                lines.push(std::mem::take(&mut current));
                current.push_str(&" ".repeat(hang));
                used = hang;
                floor = hang;
            }
        }
        let mut word = word;
        while cell_width(word) > width - used {
            let cut = fit_cells(word, width - used);
            if cut.is_empty() {
                if used > floor {
                    lines.push(std::mem::take(&mut current));
                    current.push_str(&" ".repeat(hang));
                    used = hang;
                    floor = hang;
                    continue;
                }
                // Not even one glyph fits this row: mark the cut and move on.
                let ch = word.chars().next().unwrap();
                current.push('›');
                lines.push(std::mem::take(&mut current));
                current.push_str(&" ".repeat(hang));
                used = hang;
                floor = hang;
                word = &word[ch.len_utf8()..];
                continue;
            }
            current.push_str(&cut);
            lines.push(std::mem::take(&mut current));
            current.push_str(&" ".repeat(hang));
            used = hang;
            floor = hang;
            word = &word[cut.len()..];
        }
        current.push_str(word);
        used += cell_width(word);
    }
    lines.push(current);
    lines
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
    fn interior_spacing_survives_and_breaks_drop_only_the_break_spaces() {
        assert_eq!(wrap("let x    = 1;", 40), vec!["let x    = 1;"]);
        assert_eq!(wrap("aa   bb cc", 5), vec!["aa", "bb cc"]);
        for line in wrap("a  b    c d      e", 4) {
            assert!(cell_width(&line) <= 4, "{line:?}");
        }
    }

    #[test]
    fn continuation_rows_hang_under_the_item_text() {
        assert_eq!(
            wrap_hanging("- one two three", 8, 2),
            vec!["- one", "  two", "  three"]
        );
    }

    #[test]
    fn a_too_long_word_breaks_hard_never_overflows() {
        let lines = wrap("abcdefghijklmno", 5);
        assert_eq!(lines, vec!["abcde", "fghij", "klmno"]);
        assert!(lines.iter().all(|l| l.chars().count() <= 5));
    }
}
