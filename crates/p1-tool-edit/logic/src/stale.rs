//! The stale-edit refusal with what changed (#74, ADR-0172).
//!
//! When the target changed on disk since this agent last observed it, the refusal carries a
//! unified diff from that observation to the current contents, so the model can re-issue the
//! edit against the current text instead of re-reading the whole file. The diff is of the
//! text the edit matches against: BOM stripped and line endings normalized to `\n`. It is
//! capped at [`MAX_DIFF_LINES`] lines with a note; without the observed bytes (the host kept
//! only their hash) or with a side that is not UTF-8, the refusal is the plain one.
//!
//! A refusal that shows the whole change has told the model everything the observation lacks,
//! so the caller records the current contents as observed and the re-issued edit lands without
//! a read. A capped or plain refusal records nothing: the model must still read the file.

use crate::{changed_since_observed, normalize_to_lf, strip_bom};

/// The most diff lines (hunk headers and body lines) a refusal carries.
pub const MAX_DIFF_LINES: usize = 200;
/// Unchanged lines shown around each change.
const CONTEXT_LINES: usize = 3;
/// A diff line longer than this many bytes is cut on a char boundary.
const MAX_LINE_BYTES: usize = 500;
/// The most changed lines the line matcher pairs up; past it the changed middle of the file
/// is shown as removed then added, which a capped diff shows the start of either way.
const MAX_EDIT_DISTANCE: usize = 1_000;

/// The refusal of an edit of `display`, which changed from `observed` (the last observation's
/// bytes, when the host kept them) to `current`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StaleRefusal {
    /// What the model is shown.
    pub message: String,
    /// Whether the message shows the whole change: no cap, no cut line. Only then may the
    /// caller record `current` as this agent's observation.
    pub shows_whole_change: bool,
}

/// The refusal for `display`, changed from `observed` (the last observation's bytes, when the
/// host kept them) to `current`.
pub fn stale_refusal(display: &str, observed: Option<&[u8]>, current: &[u8]) -> StaleRefusal {
    let plain = || StaleRefusal {
        message: changed_since_observed(display),
        shows_whole_change: false,
    };
    let texts = observed.and_then(|observed| {
        Some((
            std::str::from_utf8(observed).ok()?,
            std::str::from_utf8(current).ok()?,
        ))
    });
    let Some((observed, current)) = texts else {
        return plain();
    };
    let before = normalize_to_lf(strip_bom(observed).0);
    let after = normalize_to_lf(strip_bom(current).0);
    let (lines, cut_any) = unified_lines(&before, &after);
    if lines.is_empty() {
        // Only the BOM or the line endings changed: nothing the edit matches differs, but the
        // bytes a write would replace do, so the model is sent to read them.
        return plain();
    }
    let mut out = format!(
        "{display} changed on disk since you last read it. The change since then, as a unified diff of the text an edit matches:\n--- {display} (as you last read it)\n+++ {display} (on disk now)\n"
    );
    for line in lines.iter().take(MAX_DIFF_LINES) {
        out.push_str(line);
        out.push('\n');
    }
    if lines.len() > MAX_DIFF_LINES {
        out.push_str(&format!(
            "[diff capped at {MAX_DIFF_LINES} lines; {} more not shown: read {display} again for the rest]\n",
            lines.len() - MAX_DIFF_LINES
        ));
    }
    out.push_str("Re-issue the edit against the current text above, or read the file again.");
    StaleRefusal {
        message: out,
        shows_whole_change: lines.len() <= MAX_DIFF_LINES && !cut_any,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Op {
    Equal,
    Delete,
    Insert,
}

/// The hunks of a unified diff from `before` to `after`, one entry per output line, and
/// whether any line was cut.
fn unified_lines(before: &str, after: &str) -> (Vec<String>, bool) {
    // Lines keep their `\n`, so a final line without one differs from the same text with it.
    let old: Vec<&str> = before.split_inclusive('\n').collect();
    let new: Vec<&str> = after.split_inclusive('\n').collect();
    let ops = edit_script(&old, &new);

    // Each op with the line it shows: old and new indices advance as the script runs.
    let mut steps = Vec::with_capacity(ops.len());
    let (mut a, mut b) = (0usize, 0usize);
    for op in ops {
        steps.push((op, a, b));
        match op {
            Op::Equal => {
                a += 1;
                b += 1;
            }
            Op::Delete => a += 1,
            Op::Insert => b += 1,
        }
    }

    let mut out = Vec::new();
    let mut cut_any = false;
    let mut index = 0;
    while index < steps.len() {
        let Some(first_change) = steps[index..]
            .iter()
            .position(|(op, _, _)| *op != Op::Equal)
            .map(|offset| index + offset)
        else {
            break;
        };
        let start = first_change.saturating_sub(CONTEXT_LINES).max(index);
        // The hunk runs while the gap of unchanged lines to the next change is short enough
        // for their contexts to touch.
        let mut end = first_change;
        let mut cursor = first_change;
        while cursor < steps.len() {
            if steps[cursor].0 != Op::Equal {
                end = cursor + 1;
                cursor += 1;
                continue;
            }
            let run = steps[cursor..]
                .iter()
                .take_while(|(op, _, _)| *op == Op::Equal)
                .count();
            if cursor + run < steps.len() && run <= 2 * CONTEXT_LINES {
                cursor += run;
                continue;
            }
            break;
        }
        let stop = (end + CONTEXT_LINES).min(steps.len());
        let hunk = &steps[start..stop];
        let old_count = hunk.iter().filter(|(op, _, _)| *op != Op::Insert).count();
        let new_count = hunk.iter().filter(|(op, _, _)| *op != Op::Delete).count();
        let (_, old_start, new_start) = hunk[0];
        out.push(format!(
            "@@ -{} +{} @@",
            range(old_start, old_count),
            range(new_start, new_count)
        ));
        for &(op, a, b) in hunk {
            let (marker, line) = match op {
                Op::Equal => (' ', old[a]),
                Op::Delete => ('-', old[a]),
                Op::Insert => ('+', new[b]),
            };
            let ended = line.strip_suffix('\n');
            let text = ended.unwrap_or(line);
            if text.len() > MAX_LINE_BYTES {
                cut_any = true;
            }
            out.push(format!("{marker}{}", cut(text)));
            if ended.is_none() {
                out.push("\\ No newline at end of file".to_string());
            }
        }
        index = stop;
    }
    (out, cut_any)
}

/// A hunk range: one-based start and count, the start being the line before when it is empty.
fn range(start: usize, count: usize) -> String {
    match count {
        0 => format!("{start},0"),
        1 => format!("{}", start + 1),
        _ => format!("{},{count}", start + 1),
    }
}

fn cut(line: &str) -> String {
    if line.len() <= MAX_LINE_BYTES {
        return line.to_string();
    }
    let mut end = MAX_LINE_BYTES;
    while !line.is_char_boundary(end) {
        end -= 1;
    }
    format!("{} [line cut at {MAX_LINE_BYTES} bytes]", &line[..end])
}

/// The shortest edit script from `old` to `new`: the common head and tail, and Myers' greedy
/// algorithm on the middle, bounded by [`MAX_EDIT_DISTANCE`].
fn edit_script(old: &[&str], new: &[&str]) -> Vec<Op> {
    let head = old.iter().zip(new).take_while(|(a, b)| a == b).count();
    let tail = old[head..]
        .iter()
        .rev()
        .zip(new[head..].iter().rev())
        .take_while(|(a, b)| a == b)
        .count();
    let mut ops = vec![Op::Equal; head];
    let middle_old = &old[head..old.len() - tail];
    let middle_new = &new[head..new.len() - tail];
    match myers(middle_old, middle_new) {
        Some(middle) => ops.extend(middle),
        None => {
            ops.extend(std::iter::repeat_n(Op::Delete, middle_old.len()));
            ops.extend(std::iter::repeat_n(Op::Insert, middle_new.len()));
        }
    }
    ops.extend(std::iter::repeat_n(Op::Equal, tail));
    ops
}

/// Myers' O((N+M)D) diff, or `None` when the distance exceeds [`MAX_EDIT_DISTANCE`]. Each
/// round's frontier is kept (round `d` holds diagonals `-d..=d`), so memory is O(D²).
fn myers(old: &[&str], new: &[&str]) -> Option<Vec<Op>> {
    let (n, m) = (old.len() as isize, new.len() as isize);
    let max = (old.len() + new.len()).min(MAX_EDIT_DISTANCE) as isize;
    // `frontier[k + d]` of round `d` is the furthest old index on diagonal `k = x - y`.
    let mut rounds: Vec<Vec<isize>> = Vec::new();
    for d in 0..=max {
        let mut frontier = vec![0isize; (2 * d + 1) as usize];
        // The previous round's furthest old index on diagonal `k`.
        let previous = |k: isize| rounds[(d - 1) as usize][(k + d - 1) as usize];
        let mut k = -d;
        while k <= d {
            let mut x = if d == 0 {
                0
            } else if k == -d || (k != d && previous(k - 1) < previous(k + 1)) {
                previous(k + 1)
            } else {
                previous(k - 1) + 1
            };
            let mut y = x - k;
            while x < n && y < m && old[x as usize] == new[y as usize] {
                x += 1;
                y += 1;
            }
            frontier[(k + d) as usize] = x;
            if x >= n && y >= m {
                rounds.push(frontier);
                return Some(backtrack(&rounds, n, m));
            }
            k += 2;
        }
        rounds.push(frontier);
    }
    None
}

/// Walk the kept frontiers back from `(n, m)` to the origin, collecting the script.
fn backtrack(rounds: &[Vec<isize>], n: isize, m: isize) -> Vec<Op> {
    let mut ops = Vec::new();
    let (mut x, mut y) = (n, m);
    for d in (1..rounds.len() as isize).rev() {
        let previous = &rounds[(d - 1) as usize];
        let at = |k: isize| previous[(k + d - 1) as usize];
        let k = x - y;
        let down = k == -d || (k != d && at(k - 1) < at(k + 1));
        let previous_k = if down { k + 1 } else { k - 1 };
        let previous_x = at(previous_k);
        let previous_y = previous_x - previous_k;
        let (start_x, start_y) = if down {
            (previous_x, previous_y + 1)
        } else {
            (previous_x + 1, previous_y)
        };
        while x > start_x && y > start_y {
            ops.push(Op::Equal);
            x -= 1;
            y -= 1;
        }
        ops.push(if down { Op::Insert } else { Op::Delete });
        x = previous_x;
        y = previous_y;
    }
    while x > 0 && y > 0 {
        ops.push(Op::Equal);
        x -= 1;
        y -= 1;
    }
    ops.reverse();
    ops
}

#[cfg(test)]
mod tests {
    use super::*;

    fn diff(before: &str, after: &str) -> String {
        stale_refusal("d.txt", Some(before.as_bytes()), after.as_bytes()).message
    }

    fn whole(before: &str, after: &str) -> bool {
        stale_refusal("d.txt", Some(before.as_bytes()), after.as_bytes()).shows_whole_change
    }

    fn body(message: &str) -> Vec<&str> {
        let lines: Vec<&str> = message.lines().collect();
        lines[3..lines.len() - 1].to_vec()
    }

    #[test]
    fn an_appended_line_is_shown_with_its_context() {
        let message = diff("one\ntwo\n", "one\ntwo\nthree\n");
        assert_eq!(
            message,
            "d.txt changed on disk since you last read it. The change since then, as a unified diff of the text an edit matches:\n\
             --- d.txt (as you last read it)\n\
             +++ d.txt (on disk now)\n\
             @@ -1,2 +1,3 @@\n one\n two\n+three\n\
             Re-issue the edit against the current text above, or read the file again."
        );
        assert!(whole("one\ntwo\n", "one\ntwo\nthree\n"));
    }

    #[test]
    fn distant_changes_are_separate_hunks_with_current_line_numbers() {
        let before: String = (1..=30).map(|n| format!("line {n}\n")).collect();
        let after = before
            .replace("line 2\n", "LINE 2\n")
            .replace("line 25\n", "line 25\ninserted\n");
        let message = diff(&before, &after);
        assert_eq!(
            body(&message),
            vec![
                "@@ -1,5 +1,5 @@",
                " line 1",
                "-line 2",
                "+LINE 2",
                " line 3",
                " line 4",
                " line 5",
                "@@ -23,6 +23,7 @@",
                " line 23",
                " line 24",
                " line 25",
                "+inserted",
                " line 26",
                " line 27",
                " line 28",
            ]
        );
    }

    #[test]
    fn close_changes_share_one_hunk_and_removals_are_shown() {
        let message = diff("a\nb\nc\nd\ne\nf\ng\n", "a\nc\nd\ne\nf\nG\n");
        assert_eq!(
            body(&message),
            vec![
                "@@ -1,7 +1,6 @@",
                " a",
                "-b",
                " c",
                " d",
                " e",
                " f",
                "-g",
                "+G"
            ]
        );
    }

    #[test]
    fn a_missing_final_newline_is_marked() {
        let message = diff("one\ntwo", "one\ntwo\n");
        assert_eq!(
            body(&message),
            vec![
                "@@ -1,2 +1,2 @@",
                " one",
                "-two",
                "\\ No newline at end of file",
                "+two"
            ]
        );
    }

    #[test]
    fn emptied_and_created_contents_have_empty_ranges() {
        assert_eq!(body(&diff("x\n", "")), vec!["@@ -1 +0,0 @@", "-x"]);
        assert_eq!(body(&diff("", "x\n")), vec!["@@ -0,0 +1 @@", "+x"]);
    }

    #[test]
    fn the_diff_is_of_the_matched_text_so_endings_and_bom_alone_change_nothing() {
        let plain = changed_since_observed("d.txt");
        assert_eq!(diff("one\ntwo\n", "\u{FEFF}one\r\ntwo\r\n"), plain);
        assert!(!whole("one\ntwo\n", "one\r\ntwo\r\n"));
        assert_eq!(
            body(&diff("one\r\ntwo\r\n", "one\r\nTWO\r\n")),
            vec!["@@ -1,2 +1,2 @@", " one", "-two", "+TWO"]
        );
    }

    #[test]
    fn without_observed_bytes_or_with_binary_contents_the_refusal_is_the_plain_one() {
        let plain = StaleRefusal {
            message: changed_since_observed("d.txt"),
            shows_whole_change: false,
        };
        assert_eq!(stale_refusal("d.txt", None, b"one\n"), plain);
        assert_eq!(
            stale_refusal("d.txt", Some(&[0xff, b'\n']), b"one\n"),
            plain
        );
        assert_eq!(stale_refusal("d.txt", Some(b"one\n"), &[0xff]), plain);
    }

    #[test]
    fn a_large_change_is_capped_with_a_note() {
        let before: String = (0..300).map(|n| format!("old {n}\n")).collect();
        let after: String = (0..300).map(|n| format!("new {n}\n")).collect();
        let message = diff(&before, &after);
        let lines: Vec<&str> = message.lines().collect();
        // Three header lines, the capped body, the note and the closing line.
        assert_eq!(lines.len(), 3 + MAX_DIFF_LINES + 2);
        assert_eq!(lines[3], "@@ -1,300 +1,300 @@");
        assert_eq!(
            lines[3 + MAX_DIFF_LINES],
            "[diff capped at 200 lines; 401 more not shown: read d.txt again for the rest]"
        );
        assert!(!whole(&before, &after));
    }

    #[test]
    fn a_change_past_the_matcher_bound_is_still_a_correct_diff() {
        let before: String = (0..700).map(|n| format!("a{n}\nkeep\n")).collect();
        let after: String = (0..700).map(|n| format!("b{n}\nkeep\n")).collect();
        let old: Vec<&str> = before.split_inclusive('\n').collect();
        let new: Vec<&str> = after.split_inclusive('\n').collect();
        assert!(myers(&old, &new).is_none());
        let ops = edit_script(&old, &new);
        assert_eq!(
            ops.iter().filter(|op| **op != Op::Insert).count(),
            old.len()
        );
        assert_eq!(
            ops.iter().filter(|op| **op != Op::Delete).count(),
            new.len()
        );
    }

    #[test]
    fn a_long_line_is_cut_on_a_char_boundary() {
        let long = "é".repeat(400);
        let message = diff("x\n", &format!("{long}\n"));
        let added = body(&message)[2];
        assert!(added.starts_with("+é"), "{added}");
        assert!(added.ends_with(" [line cut at 500 bytes]"), "{added}");
        assert!(added.len() < 530);
        assert!(!whole("x\n", &format!("{long}\n")));
        assert!(whole("x\n", &format!("{}\n", "é".repeat(250))));
    }

    #[test]
    fn the_script_is_minimal_and_reconstructs_both_sides() {
        let old = ["a", "b", "c", "a", "b", "b", "a"];
        let new = ["c", "b", "a", "b", "a", "c"];
        let ops = myers(&old, &new).unwrap();
        // The classic Myers example: distance 5.
        assert_eq!(ops.iter().filter(|op| **op != Op::Equal).count(), 5);
        let (mut a, mut b) = (0, 0);
        for op in ops {
            match op {
                Op::Equal => {
                    assert_eq!(old[a], new[b]);
                    a += 1;
                    b += 1;
                }
                Op::Delete => a += 1,
                Op::Insert => b += 1,
            }
        }
        assert_eq!((a, b), (old.len(), new.len()));
    }
}
