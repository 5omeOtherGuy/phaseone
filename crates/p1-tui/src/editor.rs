//! The composer's text model: the operator's multiline draft and every edit on
//! it. One row map serves painting, the caret and Up/Down, so what moves is
//! exactly what is drawn (Iris `editor_visual_rows`). Rows break at words and
//! fall back to a glyph break for a word longer than a row; the caret and every
//! deletion step over whole grapheme clusters.

use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

/// Undo steps kept (ratatui-textarea's default depth is 50), and the memory
/// they may hold: each step is a whole draft, so a huge paste keeps few.
const UNDO_DEPTH: usize = 100;
const UNDO_BYTES: usize = 4 << 20;

/// The composer: the operator's multiline input. In focus mode it hides while
/// empty; the first edit reveals it (input drives disclosure).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Composer {
    pub text: String,
    /// Cursor as a CHAR index into `text`, always on a grapheme boundary.
    pub cursor: usize,
    /// Revealed while focus mode would hide it (an edit, a paste, `esc` down).
    pub revealed: bool,
    /// The display column Up/Down aim for while moving through rows.
    pub goal_col: Option<usize>,
    /// The last killed text (`^U`, `^K`, word deletes), for `^Y`.
    pub kill: String,
    /// Cells per row at the last frame; 0 before the first frame (no wrap).
    pub room: usize,
    /// The first visible row, as (logical line, row within it): the viewport
    /// moves only when the caret would leave it.
    pub scroll: (usize, usize),
    pub(crate) undo: Vec<(String, usize)>,
    /// Consecutive typing shares one undo step.
    pub(crate) typing: bool,
}

/// One painted row of a logical line: a byte range of `text`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Row {
    /// Logical line index (0-based).
    pub line: usize,
    /// Row index within its logical line.
    pub index: usize,
    pub start: usize,
    pub end: usize,
    /// The last row of its logical line (the caret may sit at `end`).
    pub last: bool,
}

/// Byte ranges of the logical lines of `text`, `\n` excluded.
pub fn logical_lines(text: &str) -> Vec<(usize, usize)> {
    let mut out = vec![];
    let mut start = 0;
    for (i, b) in text.bytes().enumerate() {
        if b == b'\n' {
            out.push((start, i));
            start = i + 1;
        }
    }
    out.push((start, text.len()));
    out
}

/// Rows of one logical line `[start, end)` at `room` cells (0 = never wrap).
pub fn line_rows(text: &str, line: usize, start: usize, end: usize, room: usize) -> Vec<Row> {
    let mut rows = vec![];
    let row = |index, s, e, last| Row {
        line,
        index,
        start: s,
        end: e,
        last,
    };
    if room == 0 {
        return vec![row(0, start, end, true)];
    }
    let mut row_start = start;
    let mut used = 0;
    // Byte just after the last space in this row that has content before it.
    let mut breakable: Option<usize> = None;
    let mut content = false;
    for (offset, g) in text[start..end].grapheme_indices(true) {
        let at = start + offset;
        let w = g.width();
        let space = g == " " || g == "\t";
        // Spaces may hang past the edge: a word that exactly fills a row
        // stays put when a space follows it.
        if !space && used + w > room && at > row_start {
            let cut = breakable.filter(|b| *b > row_start).unwrap_or(at);
            rows.push(row(rows.len(), row_start, cut, false));
            used = text[cut..at].width();
            row_start = cut;
            breakable = None;
            content = used > 0;
        }
        used += w;
        if g == " " || g == "\t" {
            if content {
                breakable = Some(at + g.len());
            }
        } else {
            content = true;
        }
    }
    rows.push(row(rows.len(), row_start, end, true));
    rows
}

/// Every row of `text` (small drafts and tests; frames lay out a window).
pub fn rows(text: &str, room: usize) -> Vec<Row> {
    logical_lines(text)
        .into_iter()
        .enumerate()
        .flat_map(|(i, (s, e))| line_rows(text, i, s, e, room))
        .collect()
}

/// The row holding byte `at` among `rows` (one logical line's rows or more).
pub fn row_of(rows: &[Row], at: usize) -> usize {
    rows.iter()
        .position(|r| at >= r.start && (at < r.end || (r.last && at == r.end)))
        .unwrap_or(rows.len().saturating_sub(1))
}

/// Strip terminal escape sequences (CSI, OSC, DCS…) and controls from pasted
/// text, keeping newlines; tabs become four spaces (a tab has no width here).
pub fn sanitize(text: &str) -> String {
    let text = text.replace("\r\n", "\n").replace('\r', "\n");
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\x1b' => match chars.next() {
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
            },
            // 8-bit CSI / OSC introducers.
            '\u{9b}' => {
                for c in chars.by_ref() {
                    if ('@'..='~').contains(&c) {
                        break;
                    }
                }
            }
            '\u{9d}' => {
                for c in chars.by_ref() {
                    if c == '\x07' || c == '\u{9c}' {
                        break;
                    }
                }
            }
            '\t' => out.push_str("    "),
            '\n' => out.push('\n'),
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out
}

impl Composer {
    fn byte_of(&self, char_index: usize) -> usize {
        self.text
            .char_indices()
            .nth(char_index)
            .map(|(i, _)| i)
            .unwrap_or(self.text.len())
    }

    fn byte_index(&self) -> usize {
        self.byte_of(self.cursor)
    }

    fn set_byte_cursor(&mut self, byte: usize) {
        self.cursor = self.text[..byte].chars().count();
    }

    /// Record an undo step before a change. Typing runs share one step.
    fn snapshot(&mut self, typing: bool) {
        if !(typing && self.typing) {
            if self.undo.last().map(|(t, _)| t) != Some(&self.text) {
                self.undo.push((self.text.clone(), self.cursor));
            }
            while self.undo.len() > UNDO_DEPTH
                || (self.undo.len() > 1
                    && self.undo.iter().map(|(t, _)| t.len()).sum::<usize>() > UNDO_BYTES)
            {
                self.undo.remove(0);
            }
        }
        self.typing = typing;
    }

    fn changed(&mut self) {
        self.goal_col = None;
        // Focus mode: an emptied composer folds away again (SPEC §4.3a).
        if self.text.is_empty() {
            self.revealed = false;
        }
    }

    pub fn insert_text(&mut self, text: &str) {
        let text = sanitize(text);
        if text.is_empty() {
            return;
        }
        self.snapshot(false);
        let byte = self.byte_index();
        self.text.insert_str(byte, &text);
        self.cursor += text.chars().count();
        self.changed();
        self.revealed = true;
    }

    pub fn insert(&mut self, ch: char) {
        self.snapshot(!ch.is_whitespace());
        let byte = self.byte_index();
        self.text.insert(byte, ch);
        self.cursor += 1;
        self.changed();
        self.revealed = true;
    }

    /// Delete the grapheme before the caret.
    pub fn backspace(&mut self) {
        let byte = self.byte_index();
        let Some(len) = self.text[..byte].graphemes(true).next_back().map(str::len) else {
            return;
        };
        self.snapshot(false);
        let start = byte - len;
        self.text.replace_range(start..byte, "");
        self.set_byte_cursor(start);
        self.changed();
    }

    /// Delete the grapheme after the caret.
    pub fn delete(&mut self) {
        let byte = self.byte_index();
        let Some(len) = self.text[byte..].graphemes(true).next().map(str::len) else {
            return;
        };
        self.snapshot(false);
        self.text.replace_range(byte..byte + len, "");
        self.changed();
    }

    pub fn left(&mut self) {
        let byte = self.byte_index();
        if let Some(g) = self.text[..byte].graphemes(true).next_back() {
            self.set_byte_cursor(byte - g.len());
        }
        self.goal_col = None;
        self.typing = false;
    }

    pub fn right(&mut self) {
        let byte = self.byte_index();
        if let Some(g) = self.text[byte..].graphemes(true).next() {
            self.set_byte_cursor(byte + g.len());
        }
        self.goal_col = None;
        self.typing = false;
    }

    /// Replace the whole draft (history recall, restored text) with the caret at
    /// the end. One undo step.
    pub fn set_text(&mut self, text: String) {
        self.snapshot(false);
        self.text = text;
        self.cursor = self.text.chars().count();
        self.goal_col = None;
        self.revealed = true;
    }

    pub fn take(&mut self) -> String {
        self.cursor = 0;
        self.revealed = false;
        self.goal_col = None;
        self.scroll = (0, 0);
        self.typing = false;
        self.undo.clear();
        std::mem::take(&mut self.text)
    }

    /// Whether the composer occupies rows right now: always outside focus
    /// mode; inside, only once revealed or while it holds text.
    pub fn visible(&self, focus: bool) -> bool {
        !focus || self.revealed || !self.text.is_empty()
    }

    fn line_bounds(&self) -> (usize, usize) {
        let byte = self.byte_index();
        let start = self.text[..byte].rfind('\n').map_or(0, |n| n + 1);
        let end = self.text[byte..]
            .find('\n')
            .map_or(self.text.len(), |n| byte + n);
        (start, end)
    }

    pub fn home(&mut self) {
        let (start, _) = self.line_bounds();
        self.set_byte_cursor(start);
        self.goal_col = None;
        self.typing = false;
    }

    pub fn end(&mut self) {
        let (_, end) = self.line_bounds();
        self.set_byte_cursor(end);
        self.goal_col = None;
        self.typing = false;
    }

    fn kill_range(&mut self, start: usize, end: usize) {
        if start == end {
            return;
        }
        self.snapshot(false);
        self.kill = self.text[start..end].to_owned();
        self.text.replace_range(start..end, "");
        self.set_byte_cursor(start);
        self.changed();
    }

    /// `^U`: erase from the start of the line to the caret.
    pub fn clear_line(&mut self) {
        let (start, _) = self.line_bounds();
        self.kill_range(start, self.byte_index());
    }

    /// `^K`: erase to the end of the line; at the end, join the next line.
    pub fn kill_end(&mut self) {
        let byte = self.byte_index();
        let (_, end) = self.line_bounds();
        let end = if end == byte {
            (end + 1).min(self.text.len())
        } else {
            end
        };
        self.kill_range(byte, end);
    }

    fn word_left_of(&self, byte: usize) -> usize {
        let before = &self.text[..byte];
        let trimmed = before.trim_end();
        trimmed.rfind(char::is_whitespace).map_or(0, |i| {
            i + trimmed[i..].chars().next().map_or(1, char::len_utf8)
        })
    }

    fn word_right_of(&self, byte: usize) -> usize {
        let after = &self.text[byte..];
        let word = after.find(char::is_whitespace).unwrap_or(after.len());
        let rest = &after[word..];
        let gap = rest.len() - rest.trim_start().len();
        byte + word + gap
    }

    pub fn word_left(&mut self) {
        let byte = self.byte_index();
        self.set_byte_cursor(self.word_left_of(byte));
        self.goal_col = None;
        self.typing = false;
    }

    pub fn word_right(&mut self) {
        let byte = self.byte_index();
        self.set_byte_cursor(self.word_right_of(byte));
        self.goal_col = None;
        self.typing = false;
    }

    pub fn delete_word(&mut self) {
        let byte = self.byte_index();
        self.kill_range(self.word_left_of(byte), byte);
    }

    pub fn delete_word_right(&mut self) {
        let byte = self.byte_index();
        let after = &self.text[byte..];
        let gap = after.len() - after.trim_start().len();
        let word = after[gap..]
            .find(char::is_whitespace)
            .unwrap_or(after.len() - gap);
        self.kill_range(byte, byte + gap + word);
    }

    /// `^Y`: insert the last killed text.
    pub fn yank(&mut self) {
        if !self.kill.is_empty() {
            let kill = self.kill.clone();
            self.snapshot(false);
            let byte = self.byte_index();
            self.text.insert_str(byte, &kill);
            self.cursor += kill.chars().count();
            self.changed();
        }
    }

    /// Undo the last edit; false when there is nothing to undo.
    pub fn undo(&mut self) -> bool {
        let Some((text, cursor)) = self.undo.pop() else {
            return false;
        };
        self.text = text;
        self.cursor = cursor.min(self.text.chars().count());
        self.typing = false;
        self.goal_col = None;
        true
    }

    /// Move the caret one painted row up (`-1`) or down (`1`), keeping the
    /// display column. False at the first/last row: the caller may go to history.
    pub fn move_row(&mut self, dir: isize) -> bool {
        let byte = self.byte_index();
        let lines = logical_lines(&self.text);
        let line = lines
            .iter()
            .position(|(s, e)| byte >= *s && byte <= *e)
            .unwrap_or(0);
        let here = line_rows(&self.text, line, lines[line].0, lines[line].1, self.room);
        let r = row_of(&here, byte);
        let col = self
            .goal_col
            .unwrap_or_else(|| self.text[here[r].start..byte].width());
        let target = if dir < 0 {
            if r > 0 {
                Some(here[r - 1])
            } else if line > 0 {
                line_rows(
                    &self.text,
                    line - 1,
                    lines[line - 1].0,
                    lines[line - 1].1,
                    self.room,
                )
                .last()
                .copied()
            } else {
                None
            }
        } else if r + 1 < here.len() {
            Some(here[r + 1])
        } else if line + 1 < lines.len() {
            line_rows(
                &self.text,
                line + 1,
                lines[line + 1].0,
                lines[line + 1].1,
                self.room,
            )
            .first()
            .copied()
        } else {
            None
        };
        let Some(target) = target else {
            return false;
        };
        let mut at = target.start;
        let mut used = 0;
        for (offset, g) in self.text[target.start..target.end].grapheme_indices(true) {
            let w = g.width();
            if used + w > col {
                break;
            }
            used += w;
            at = target.start + offset + g.len();
        }
        // A soft-wrapped row's end is the next row's start: stay on this row.
        if !target.last && at == target.end && at > target.start {
            at = target.start
                + self.text[target.start..target.end]
                    .grapheme_indices(true)
                    .next_back()
                    .map_or(0, |(i, _)| i);
        }
        self.set_byte_cursor(at);
        self.goal_col = Some(col);
        self.typing = false;
        true
    }

    /// Put the caret at a painted position (a click): `row` and `col` relative
    /// to `rows`.
    pub fn place(&mut self, rows: &[Row], row: usize, col: usize) {
        let Some(target) = rows.get(row) else {
            return;
        };
        let mut at = target.start;
        let mut used = 0;
        for (offset, g) in self.text[target.start..target.end].grapheme_indices(true) {
            let w = g.width();
            if used + w > col {
                break;
            }
            used += w;
            at = target.start + offset + g.len();
        }
        // Past a soft-wrapped row's end is still that row, not the next one.
        if !target.last && at == target.end && at > target.start {
            at = target.start
                + self.text[target.start..target.end]
                    .grapheme_indices(true)
                    .next_back()
                    .map_or(0, |(i, _)| i);
        }
        self.set_byte_cursor(at);
        self.goal_col = None;
        self.typing = false;
    }

    /// Byte offset of the caret (layout works in bytes).
    pub fn caret_byte(&self) -> usize {
        self.byte_index()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn composer(text: &str, room: usize) -> Composer {
        let mut c = Composer {
            room,
            ..Composer::default()
        };
        c.insert_text(text);
        c
    }

    #[test]
    fn rows_break_at_words_and_keep_the_space_on_the_upper_row() {
        let text = "alpha beta gamma delta";
        let rows = rows(text, 11);
        let parts: Vec<&str> = rows.iter().map(|r| &text[r.start..r.end]).collect();
        assert_eq!(parts, ["alpha beta ", "gamma delta"]);
        // A word that exactly fills the row stays when a space follows it.
        let text = "alpha beta ";
        assert_eq!(super::rows(text, 10).len(), 1);
        // A word longer than a row breaks at the glyph.
        let text = "abcdefghij";
        let parts: Vec<&str> = super::rows(text, 4)
            .iter()
            .map(|r| &text[r.start..r.end])
            .collect();
        assert_eq!(parts, ["abcd", "efgh", "ij"]);
        // An exactly full row makes no phantom blank row.
        let text = "aaaa\nb";
        assert_eq!(super::rows(text, 4).len(), 2);
    }

    #[test]
    fn up_and_down_move_by_painted_row_with_a_sticky_column() {
        let mut c = composer("alpha beta gamma delta\nxy", 11);
        c.cursor = 13; // "gam|ma"
        assert!(c.move_row(-1));
        assert_eq!(c.cursor, 2); // "al|pha", column 2
        assert!(!c.move_row(-1), "first row: history may take over");
        assert!(c.move_row(1));
        assert!(c.move_row(1));
        assert_eq!(c.cursor, 25); // "xy" is short: end of it
        assert!(c.move_row(-1));
        assert_eq!(c.cursor, 13, "the goal column survives a short row");
    }

    #[test]
    fn editing_steps_over_whole_graphemes() {
        let mut c = composer("family: 👨‍👩‍👧 flag: 🇩🇪 cafe\u{301}", 0);
        c.backspace();
        assert!(c.text.ends_with("caf"), "{:?}", c.text);
        c.word_left();
        c.left();
        c.backspace();
        assert!(
            c.text.contains("flag:  caf"),
            "the flag went whole: {:?}",
            c.text
        );
        c.home();
        for _ in 0..9 {
            c.right();
        }
        c.backspace();
        assert!(c.text.starts_with("family:  "), "{:?}", c.text);
    }

    #[test]
    fn kills_yank_and_undo_restore_what_was_typed() {
        let mut c = composer("keep this line", 0);
        c.word_left();
        c.kill_end();
        assert_eq!(c.text, "keep this ");
        c.home();
        c.yank();
        assert_eq!(c.text, "linekeep this ");
        assert!(c.undo());
        assert_eq!(c.text, "keep this ");
        assert!(c.undo());
        assert_eq!(c.text, "keep this line");
        for ch in "abc".chars() {
            c.insert(ch);
        }
        assert!(c.undo(), "a typing run is one step");
        assert_eq!(c.text, "keep this line");
    }

    #[test]
    fn pasted_escape_sequences_leave_no_residue() {
        assert_eq!(
            sanitize("\x1b[31mred\x1b[0m\ttab\r\nnext\x1b]8;;http://x\x07link\x1b]8;;\x07"),
            "red    tab\nnextlink"
        );
    }
}
