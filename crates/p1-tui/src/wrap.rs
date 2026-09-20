//! Word wrapping for the transcript. Breaks at spaces; a word longer than the
//! width breaks hard rather than overflowing (SPEC §3: nothing overflows).
//! Kept deliberately small — identifiers and paths are never reordered, only
//! broken when they cannot fit alone on a line.

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
        let word_len = word.chars().count();
        let current_len = current.chars().count();
        if current_len > 0 && current_len + 1 + word_len > body_width {
            lines.push(std::mem::take(&mut current));
        } else if current_len > 0 {
            current.push(' ');
        }
        if word_len > body_width {
            // A word that cannot fit alone breaks hard at the cell edge.
            let mut rest = word;
            while rest.chars().count() > body_width {
                let cut: String = rest.chars().take(body_width).collect();
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
    fn a_too_long_word_breaks_hard_never_overflows() {
        let lines = wrap("abcdefghijklmno", 5);
        assert_eq!(lines, vec!["abcde", "fghij", "klmno"]);
        assert!(lines.iter().all(|l| l.chars().count() <= 5));
    }
}
