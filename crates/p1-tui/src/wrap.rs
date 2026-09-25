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

/// The number of rows `wrap(text, width)` would produce. It reuses `wrap` so
/// measurement cannot drift from rendering.
pub(crate) fn wrap_len(text: &str, width: usize) -> usize {
    wrap(text, width).len()
}

struct Unit<S> {
    text: String,
    style: S,
    width: usize,
}

type UnitRow<S> = Vec<Unit<S>>;

fn char_width(ch: char) -> usize {
    unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0)
}

fn units_for_text<S: Copy>(text: &str, style: S) -> Vec<Unit<S>> {
    let mut units: Vec<Unit<S>> = Vec::new();
    for ch in text.chars() {
        let width = char_width(ch);
        if width == 0 && !units.is_empty() {
            units.last_mut().expect("non-empty units").text.push(ch);
        } else {
            units.push(Unit {
                text: ch.to_string(),
                style,
                width,
            });
        }
    }
    units
}

// Adapted from iris-donor/src/ui/textengine.rs (pin 5b04a1ad3412ad0bb663b6355f77a024aec0ddfa, MIT).
// The donor's bounded-progress wrapping is retained, while oversized clusters
// become an ellipsis because SLAB forbids terminal overflow.
fn wrap_units<S: Copy>(text: &str, width: usize, style: S) -> Vec<Vec<Unit<S>>> {
    if width == 0 {
        return vec![Vec::new()];
    }
    let leading_spaces = text.chars().take_while(|ch| *ch == ' ').count();
    let body = &text[leading_spaces..];
    let indent = leading_spaces.min(width - 1);
    let body_width = width - indent;
    let mut body_rows = Vec::new();
    let mut current: Vec<Unit<S>> = Vec::new();
    let mut current_width = 0usize;
    let words = body
        .split(' ')
        .filter(|word| !word.is_empty())
        .map(|word| (cell_width(word), units_for_text(word, style)))
        .collect::<Vec<_>>();
    let push_current = |rows: &mut Vec<UnitRow<S>>, current: &mut UnitRow<S>| {
        if !current.is_empty() {
            rows.push(std::mem::take(current));
        }
    };
    for (word_width, word) in words {
        if !current.is_empty() && current_width + 1 + word_width > body_width {
            push_current(&mut body_rows, &mut current);
            current_width = 0;
        }
        if !current.is_empty() {
            current.push(Unit {
                text: " ".to_string(),
                style: current.last().expect("non-empty current").style,
                width: 1,
            });
            current_width += 1;
        }
        if word_width <= body_width {
            current_width += word_width;
            current.extend(word);
            continue;
        }
        for unit in word {
            if unit.width > body_width {
                push_current(&mut body_rows, &mut current);
                current.push(Unit {
                    text: "…".to_string(),
                    style: unit.style,
                    width: 1,
                });
                push_current(&mut body_rows, &mut current);
                current_width = 0;
            } else {
                if !current.is_empty() && current_width + unit.width > body_width {
                    push_current(&mut body_rows, &mut current);
                    current_width = 0;
                }
                current_width += unit.width;
                current.push(unit);
            }
        }
    }
    if !current.is_empty() || body_rows.is_empty() {
        body_rows.push(current);
    }
    let indent_units = || {
        (0..indent)
            .map(|_| Unit {
                text: " ".to_string(),
                style,
                width: 1,
            })
            .collect::<Vec<_>>()
    };
    body_rows
        .into_iter()
        .map(|mut row| {
            let mut with_indent = indent_units();
            with_indent.append(&mut row);
            with_indent
        })
        .collect()
}

/// Wrap `text` to `width` columns, returning the lines. Empty input yields one
/// empty line so a block always occupies its row. Leading indentation is
/// preserved and hangs: continuation lines keep the same indent.
pub fn wrap(text: &str, width: usize) -> Vec<String> {
    wrap_units(text, width, ())
        .into_iter()
        .map(|row| row.into_iter().map(|unit| unit.text).collect())
        .collect()
}

/// `wrap` for text that may hold newlines: each line wraps on its own.
pub fn wrap_paragraphs(text: &str, width: usize) -> Vec<String> {
    text.split('\n')
        .flat_map(|line| wrap(line, width))
        .collect()
}

/// The number of rows `wrap_paragraphs(text, width)` would produce.
pub(crate) fn wrap_paragraphs_len(text: &str, width: usize) -> usize {
    text.split('\n').map(|line| wrap_len(line, width)).sum()
}

/// `wrap` over styled runs. Wrapping only drops and re-inserts spaces, so the
/// wrapped rows hold the text's other characters in their original order and
/// each gets its run's style back; a space takes the style before it (a blank
/// cell shows no colour). The row count is `wrap_len` of the joined text.
pub fn wrap_styled<S: Copy + PartialEq>(
    runs: &[(String, S)],
    width: usize,
) -> Vec<Vec<(String, S)>> {
    let Some(first) = runs.first().map(|(_, style)| *style) else {
        return vec![Vec::new()];
    };
    let plain: String = runs.iter().map(|(text, _)| text.as_str()).collect();
    let mut styles = runs
        .iter()
        .flat_map(|(text, style)| text.chars().filter(|c| *c != ' ').map(move |_| *style));
    wrap(&plain, width)
        .into_iter()
        .map(|line| {
            let mut row: Vec<(String, S)> = Vec::new();
            let mut style = first;
            for ch in line.chars() {
                if ch != ' ' {
                    style = styles.next().unwrap_or(style);
                }
                match row.last_mut() {
                    Some((text, last)) if *last == style || ch == ' ' => text.push(ch),
                    _ => row.push((ch.to_string(), style)),
                }
            }
            row
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn styled_wrap_keeps_each_word_in_its_run() {
        let runs = vec![
            ("see ".to_string(), 'i'),
            ("a/b.rs".to_string(), 'r'),
            (" now".to_string(), 'i'),
        ];
        let rows = wrap_styled(&runs, 6);
        assert_eq!(
            rows,
            vec![
                vec![("see".to_string(), 'i')],
                vec![("a/b.rs".to_string(), 'r')],
                vec![("now".to_string(), 'i')],
            ]
        );
        assert_eq!(rows.len(), wrap_len("see a/b.rs now", 6));
    }

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
    fn oversized_glyph_is_one_ellipsis_and_returns() {
        let (rows, len) = watchdog(|| (wrap("界", 1), wrap_len("界", 1)));
        assert_eq!(rows, vec!["…"]);
        assert_eq!(len, 1);
    }

    #[test]
    fn oversized_glyph_absorbs_following_zero_width_char() {
        let (rows, len) = watchdog(|| (wrap("界\u{301}", 1), wrap_len("界\u{301}", 1)));
        assert_eq!(rows, vec!["…"]);
        assert_eq!(len, 1);
    }

    #[test]
    fn oversized_glyph_after_indent_is_one_ellipsis_and_returns() {
        let (rows, len) = watchdog(|| (wrap(" 界", 2), wrap_len(" 界", 2)));
        assert_eq!(rows, vec![" …"]);
        assert_eq!(len, 1);
    }

    #[test]
    fn oversized_glyphs_advance_between_other_characters() {
        let (rows, len) = watchdog(|| (wrap("a界b", 1), wrap_len("a界b", 1)));
        assert_eq!(rows, vec!["a", "…", "b"]);
        assert_eq!(len, 3);
    }

    #[test]
    fn a_wide_glyph_that_fits_alone_is_not_replaced() {
        let (rows, len) = watchdog(|| (wrap("界界", 3), wrap_len("界界", 3)));
        assert_eq!(rows, vec!["界", "界"]);
        assert_eq!(len, 2);
    }

    #[test]
    fn indentation_is_capped_to_leave_body_cells() {
        let (rows, narrow) = watchdog(|| (wrap("    ab", 3), wrap("  ab", 1)));
        assert_eq!(rows, vec!["  a", "  b"]);
        assert_eq!(narrow, vec!["a", "b"]);
    }

    #[test]
    fn combining_marks_stay_with_their_base() {
        let (rows, len) =
            watchdog(|| (wrap("e\u{301}e\u{301}", 1), wrap_len("e\u{301}e\u{301}", 1)));
        assert_eq!(rows, vec!["e\u{301}", "e\u{301}"]);
        assert_eq!(len, 2);
    }

    #[test]
    fn a_fitting_zwj_word_is_not_split_by_character_widths() {
        let family = "👨\u{200d}👩\u{200d}👧";
        let text = format!("x{family}");
        let expected = text.clone();
        let rows = watchdog(move || wrap(&text, 3));
        assert_eq!(rows, vec![expected]);
    }

    #[test]
    fn styled_oversized_glyph_keeps_its_style() {
        let runs = vec![("a".into(), 'x'), ("界".into(), 'y')];
        let rows = watchdog(move || wrap_styled(&runs, 1));
        assert_eq!(rows, vec![vec![("a".into(), 'x')], vec![("…".into(), 'y')]]);
    }

    #[test]
    fn wrap_and_len_are_bounded_for_all_corpus_widths() {
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
            "界",
            " 界",
            "a界b",
            "    ab",
            "e\u{301}e\u{301}",
        ];
        for text in corpus {
            for width in 0..=14 {
                let (rows, len) = watchdog(move || (wrap(text, width), wrap_len(text, width)));
                assert_eq!(
                    len,
                    rows.len(),
                    "wrap_len disagrees for {text:?} at {width}"
                );
                if width > 0 {
                    assert!(
                        rows.iter().all(|row| cell_width(row) <= width),
                        "row overflows for {text:?} at {width}: {rows:?}"
                    );
                }
            }
        }
    }

    fn watchdog<T, F>(f: F) -> T
    where
        F: FnOnce() -> T + Send + 'static,
        T: Send + 'static,
    {
        let (sender, receiver) = std::sync::mpsc::channel();
        let _ = std::thread::spawn(move || {
            let _ = sender.send(f());
        });
        receiver
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("wrap watchdog expired")
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
        for text in corpus {
            for width in 0..=14 {
                assert_eq!(
                    wrap_len(text, width),
                    wrap(text, width).len(),
                    "wrap_len disagrees with wrap for {text:?} at {width}"
                );
            }
        }
    }
}
