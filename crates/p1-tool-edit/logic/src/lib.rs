//! The `edit` tool's pure logic: the model-facing declaration, input validation, the
//! exact-match replacement with its BOM and line-ending handling, the output text and the
//! call and result descriptions.
//!
//! It is the one copy of that logic: the native `p1-tool-edit` and the component
//! `modules/p1-module-edit/` both call it, so their texts cannot drift apart (decision S0-R3
//! in `docs/design/modules/package.md`). It is target-independent by construction: std,
//! serde and serde_json only, and no filesystem, thread or clock. Everything that touches a
//! file goes through the capabilities [`exec`] is handed.

pub mod exec;
pub mod wire;

use serde::Deserialize;

/// The default model-facing tool name.
pub const NAME: &str = "edit";
/// The default model-facing description.
pub const DESCRIPTION: &str = "Replace a string in an existing workspace file.\n`old_string` is matched exactly first; when nothing matches exactly, a whitespace- and Unicode-tolerant fallback (Unicode spaces, curly quotes, Unicode dashes, trailing whitespace) is tried and the applied region is echoed back. It must match uniquely unless `replace_all` is set.\nSeveral replacements in one file go in one call: pass `edits`, a list of {old_string, new_string, replace_all?} entries, instead of old_string/new_string/replace_all. Each entry is matched against the file as it was before the call, matches must not overlap, and if any entry fails nothing is written. Example: {\"file_path\": \"src/a.rs\", \"edits\": [{\"old_string\": \"fn old()\", \"new_string\": \"fn new()\"}, {\"old_string\": \"old();\", \"new_string\": \"new();\"}]}\nRead the file first: the edit is refused if you have never read it, or if it changed on disk since you did.\nThe file's line endings and final newline are preserved.";
/// The call-description verb (ADR-0057), one of the closed vocabulary of `protocol.md`.
pub const VERB: &str = "edit";
/// The most output bytes the model is shown.
pub const MAX_OUTPUT_BYTES: usize = 50_000;
/// The most output lines the model is shown.
pub const MAX_OUTPUT_LINES: usize = 2_000;
/// What joins the entries of a list-form call in its preview and its result diff: a line of
/// its own, so it shows as unchanged context between the replaced texts.
pub const ENTRY_SEPARATOR: &str = "\n…\n";

/// The input JSON Schema of the declaration.
pub fn input_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "file_path": {
                "type": "string",
                "description": "File path, relative to the workspace root or absolute inside it."
            },
            "old_string": {
                "type": "string",
                "minLength": 1,
                "description": "Text to replace: matched exactly first, then with a whitespace/Unicode-tolerant fallback that echoes the applied region; must be unique unless replace_all is set. Required unless edits is given."
            },
            "new_string": {
                "type": "string",
                "description": "Replacement text. Identical to old_string means no change. Required unless edits is given."
            },
            "replace_all": {
                "type": "boolean",
                "default": false,
                "description": "Replace every occurrence instead of requiring a unique match."
            },
            "edits": {
                "type": "array",
                "minItems": 1,
                "description": "Several replacements in this one file, instead of old_string/new_string/replace_all. All or nothing: each entry is matched against the file as it was before the call, matches must not overlap, and if any entry fails nothing is written.",
                "items": {
                    "type": "object",
                    "properties": {
                        "old_string": {"type": "string", "minLength": 1},
                        "new_string": {"type": "string"},
                        "replace_all": {"type": "boolean", "default": false}
                    },
                    "required": ["old_string", "new_string"],
                    "additionalProperties": false
                }
            }
        },
        "required": ["file_path"],
        "additionalProperties": false
    })
}

/// One replacement: the single form's fields, or one entry of the `edits` list.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Replacement {
    pub old_string: String,
    pub new_string: String,
    #[serde(default)]
    pub replace_all: bool,
}

/// A validated `edit` input: one file and its replacements in call order. The single form
/// is one replacement with `listed` false; the `edits` form sets `listed`, so its messages
/// name the failing entry by index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditInput {
    pub file_path: String,
    pub edits: Vec<Replacement>,
    pub listed: bool,
}

impl EditInput {
    /// The single-replacement form.
    pub fn single(file_path: &str, old_string: &str, new_string: &str, replace_all: bool) -> Self {
        Self {
            file_path: file_path.to_string(),
            edits: vec![Replacement {
                old_string: old_string.to_string(),
                new_string: new_string.to_string(),
                replace_all,
            }],
            listed: false,
        }
    }

    /// The replaced and the replacing text, as a call preview and a result diff show them:
    /// the single form's two strings, or every entry's joined by [`ENTRY_SEPARATOR`].
    pub fn before_and_after(&self) -> (String, String) {
        let join = |pick: fn(&Replacement) -> &str| {
            self.edits
                .iter()
                .map(pick)
                .collect::<Vec<_>>()
                .join(ENTRY_SEPARATOR)
        };
        (
            join(|entry| &entry.old_string),
            join(|entry| &entry.new_string),
        )
    }
}

/// The wire shape, before the two forms are told apart.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawInput {
    file_path: String,
    old_string: Option<String>,
    new_string: Option<String>,
    replace_all: Option<bool>,
    edits: Option<Vec<Replacement>>,
}

/// The single form as it was parsed before the list form existed, so its input errors
/// (a missing or mistyped field, with serde's position) read exactly as they did.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SingleInput {
    file_path: String,
    old_string: String,
    new_string: String,
    #[serde(default)]
    replace_all: bool,
    /// Always absent or null here: a present list takes the list form.
    #[serde(default, rename = "edits")]
    _edits: Option<serde::de::IgnoredAny>,
}

/// Parse and validate a JSON input; `tool` is the name the model called, for the message.
pub fn parse_json_input(tool: &str, raw: &str) -> Result<EditInput, String> {
    let parse_error = |error: serde_json::Error| invalid(tool, &error.to_string());
    let input: RawInput = serde_json::from_str(raw).map_err(parse_error)?;
    let Some(edits) = input.edits else {
        let input: SingleInput = serde_json::from_str(raw).map_err(parse_error)?;
        if input.old_string.is_empty() {
            return Err(invalid(tool, "`old_string` must not be empty"));
        }
        return Ok(EditInput::single(
            &input.file_path,
            &input.old_string,
            &input.new_string,
            input.replace_all,
        ));
    };
    // `replace_all: false` is the schema's default, which a model may spell out.
    if input.old_string.is_some() || input.new_string.is_some() || input.replace_all == Some(true) {
        return Err(invalid(
            tool,
            "give either `edits` or `old_string`/`new_string`/`replace_all`, not both",
        ));
    }
    if edits.is_empty() {
        return Err(invalid(tool, "`edits` must not be empty"));
    }
    if let Some(index) = edits.iter().position(|entry| entry.old_string.is_empty()) {
        return Err(invalid(
            tool,
            &format!("`edits[{index}].old_string` must not be empty"),
        ));
    }
    Ok(EditInput {
        file_path: input.file_path,
        edits,
        listed: true,
    })
}

/// Whether the call changes nothing: every replacement's two strings are identical. Such a
/// call succeeds without touching the file and without asking the host for its read state.
pub fn is_no_change(input: &EditInput) -> bool {
    input
        .edits
        .iter()
        .all(|entry| entry.old_string == entry.new_string)
}

/// The model-facing text of a no-op call, naming the path unchanged.
pub fn no_change(input: &EditInput) -> String {
    let file_path = &input.file_path;
    if input.listed {
        format!(
            "No change: every entry's old_string and new_string are identical; {file_path} was not modified."
        )
    } else {
        format!("No change: old_string and new_string are identical; {file_path} was not modified.")
    }
}

/// The error for a freeform text input, which this function tool never accepts.
pub fn text_input_error(tool: &str) -> String {
    invalid(tool, "expected a JSON object input, got freeform text")
}

fn invalid(tool: &str, reason: &str) -> String {
    format!("Invalid input for {tool}: {reason}")
}

/// The target does not exist.
pub fn does_not_exist(display: &str) -> String {
    format!("{display} does not exist.")
}

/// The target exists but could not be read.
pub fn could_not_read(display: &str, error: &str) -> String {
    format!("{display} could not be read: {error}")
}

/// Read-before-mutate: this agent never observed the target.
pub fn never_observed(display: &str) -> String {
    format!("You must read {display} before changing it.")
}

/// Read-before-mutate: the target changed since this agent observed it.
pub fn changed_since_observed(display: &str) -> String {
    format!("{display} changed on disk since you last read it; read it again.")
}

/// The replacement could not be written.
pub fn failed_to_write(display: &str, error: &str) -> String {
    format!("failed to write {display}: {error}")
}

/// The confinement refusal, worded as the workspace service words it.
pub fn escapes_workspace(requested: &str) -> String {
    format!("path escapes workspace: {requested}")
}

/// The new contents of an edited file, how many occurrences were replaced and, when the
/// tolerant fallback applied the change, the line-numbered region it applied (ADR-0106).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Edited {
    pub contents: String,
    pub replacements: usize,
    pub applied_region: Option<String>,
}

/// Apply `input` to `bytes`, the current contents of the file shown as `display`.
///
/// Matching happens on LF-normalized text; the file's own ending is restored on write, so a
/// CRLF file stays CRLF and a missing final newline stays missing. A UTF-8 BOM is kept.
/// An exact match is tried first; when it finds nothing, a whitespace- and
/// Unicode-confusable-tolerant match is applied (ADR-0106) and its region is reported.
///
/// Every replacement is located in the file as it stands before the call, and their matched
/// ranges must not overlap; when any replacement fails, the error names it and nothing is
/// changed, so the caller writes all of them or none.
pub fn edit_text(display: &str, bytes: &[u8], input: &EditInput) -> Result<Edited, String> {
    let text = std::str::from_utf8(bytes).map_err(|_| format!("{display} is not valid UTF-8."))?;
    let (body, had_bom) = strip_bom(text);
    let ending = detect_line_ending(body);
    let (normalized, offsets) = normalized_with_offsets(body);
    let failed = |index: usize, message: String| {
        if input.listed {
            format!("edits[{index}] failed; no edit was applied. {message}")
        } else {
            message
        }
    };

    let mut spans: Vec<Span> = Vec::new();
    for (index, entry) in input.edits.iter().enumerate() {
        let old_string = normalize_to_lf(&entry.old_string);
        let Some((ranges, tolerant)) = locate(&normalized, &old_string) else {
            return Err(failed(index, not_found(display, &normalized, &old_string)));
        };
        if ranges.len() > 1 && !entry.replace_all {
            return Err(failed(
                index,
                format!(
                    "old_string occurs {} times in {display}; add context to make it unique or set replace_all.",
                    ranges.len()
                ),
            ));
        }
        spans.extend(ranges.into_iter().map(|(start, end)| Span {
            start,
            end,
            entry: index,
            tolerant,
        }));
    }
    spans.sort_by_key(|span| (span.start, span.end));
    if let Some(pair) = spans.windows(2).find(|pair| pair[1].start < pair[0].end) {
        let (first, second) = (
            pair[0].entry.min(pair[1].entry),
            pair[0].entry.max(pair[1].entry),
        );
        return Err(format!(
            "edits[{first}] and edits[{second}] overlap in {display}; no edit was applied. Merge them into one entry."
        ));
    }
    let replacements = spans.len();

    let new_strings: Vec<String> = input
        .edits
        .iter()
        .map(|entry| normalize_to_lf(&entry.new_string))
        .collect();
    let tolerant = spans.iter().any(|span| span.tolerant);
    let mut restored = String::with_capacity(body.len());
    let mut cursor = 0;
    // The echoed region is cut from an LF view of the edited text, as iris cuts it before
    // restoring line endings: a CR-only or mixed-ending file would otherwise number wrongly.
    let mut lf_view = String::new();
    let mut lf_cursor = 0;
    let mut first: Option<(usize, usize)> = None;
    for span in &spans {
        let new_string = &new_strings[span.entry];
        restored.push_str(&body[cursor..offsets[span.start]]);
        restored.push_str(&restore_line_endings(new_string, ending));
        cursor = offsets[span.end];
        if tolerant {
            lf_view.push_str(&normalized[lf_cursor..span.start]);
            let change_start = lf_view.len();
            lf_view.push_str(new_string);
            if span.tolerant {
                first.get_or_insert((change_start, lf_view.len()));
            }
            lf_cursor = span.end;
        }
    }
    restored.push_str(&body[cursor..]);
    let applied_region = first.map(|(start, end)| {
        lf_view.push_str(&normalized[lf_cursor..]);
        region_snippet(&lf_view, start, end)
    });
    let contents = if had_bom {
        format!("\u{FEFF}{restored}")
    } else {
        restored
    };
    Ok(Edited {
        contents,
        replacements,
        applied_region,
    })
}

/// One matched range of the normalized text, the replacement it belongs to, and whether the
/// tolerant fallback found it.
struct Span {
    start: usize,
    end: usize,
    entry: usize,
    tolerant: bool,
}

/// The model-facing output of a successful edit, already bounded. The applied region is
/// echoed only when the tolerant fallback fired, so an exact match stays terse.
pub fn edited_output(display: &str, replacements: usize, applied_region: Option<&str>) -> String {
    let plural = if replacements == 1 { "" } else { "s" };
    let mut message = format!("Edited {display} ({replacements} replacement{plural}).");
    if let Some(region) = applied_region {
        message.push_str("\nApplied region (tolerant match):\n");
        message.push_str(region);
    }
    bound_output(&message, MAX_OUTPUT_BYTES, MAX_OUTPUT_LINES)
}

/// The byte ranges of `needle` in `normalized`, ascending and non-overlapping, and whether
/// the tolerant fallback produced them. An exact match is preferred; only when it finds
/// nothing is the text folded and matched again, so an exactly matching call is unchanged.
fn locate(normalized: &str, needle: &str) -> Option<(Vec<(usize, usize)>, bool)> {
    let exact = find_all(normalized, needle);
    if !exact.is_empty() {
        return Some((
            exact
                .into_iter()
                .map(|start| (start, start + needle.len()))
                .collect(),
            false,
        ));
    }
    let (haystack, offsets) = normalize_tolerant(normalized);
    let (folded, _) = normalize_tolerant(needle);
    if folded.is_empty() {
        return None;
    }
    let matches: Vec<(usize, usize)> = find_all(&haystack, &folded)
        .into_iter()
        .map(|start| (offsets[start], offsets[start + folded.len()]))
        .collect();
    if matches.is_empty() {
        return None;
    }
    Some((matches, true))
}

/// The not-found error: p1's first sentence, then the file region that most resembles
/// `needle`, so the model can re-anchor without re-reading the whole file.
fn not_found(display: &str, normalized: &str, needle: &str) -> String {
    let mut message = format!("old_string was not found in {display}.");
    if let Some((line, region)) = closest_candidate_region(normalized, needle) {
        message.push_str(&format!(
            "\nClosest matching region (around line {line}):\n{region}"
        ));
    }
    message
}

/// The file line that most resembles the first non-blank line of `needle`, by shared
/// whitespace-delimited words, as its 1-based number and a numbered snippet around it.
fn closest_candidate_region(content: &str, needle: &str) -> Option<(usize, String)> {
    let target = needle.lines().find(|line| !line.trim().is_empty())?.trim();
    let target_words: Vec<&str> = target.split_whitespace().collect();
    if target_words.is_empty() {
        return None;
    }
    let mut best: Option<(usize, usize)> = None; // (score, line index)
    for (index, line) in content.split('\n').enumerate() {
        let score = line
            .split_whitespace()
            .filter(|word| target_words.contains(word))
            .count();
        if score > 0 && best.is_none_or(|(best_score, _)| score > best_score) {
            best = Some((score, index));
        }
    }
    let (_, index) = best?;
    Some((index + 1, numbered_lines(content, index, index, 2)))
}

/// A compact, line-numbered snippet of `content` spanning `[start, end)` plus two lines of
/// context, for the region a tolerant match applied.
fn region_snippet(content: &str, start: usize, end: usize) -> String {
    let start_line = content[..start.min(content.len())].matches('\n').count();
    let end_line = content[..end.min(content.len())].matches('\n').count();
    numbered_lines(content, start_line, end_line, 2)
}

/// The most lines and bytes an echoed region shows, its elision note included. A region
/// over either keeps its first and last lines and names how many between them it leaves
/// out, so a huge replacement still echoes both of its ends well inside the output bound.
pub const REGION_MAX_LINES: usize = 40;
pub const REGION_MAX_BYTES: usize = 8_000;

/// Render lines `[from - context ..= to + context]` (0-based, clamped) as `NNNN | text`,
/// with over-long lines cut, within [`REGION_MAX_LINES`] and [`REGION_MAX_BYTES`].
fn numbered_lines(content: &str, from_line: usize, to_line: usize, context: usize) -> String {
    const MAX_LINE_CHARS: usize = 200;
    let lines: Vec<&str> = content.split('\n').collect();
    let last = lines.len().saturating_sub(1);
    let from = from_line.saturating_sub(context);
    let to = (to_line + context).min(last);
    let render = |index: usize| {
        let text = lines.get(index).copied().unwrap_or("");
        let shown: String = text.chars().take(MAX_LINE_CHARS).collect();
        let ellipsis = if text.chars().count() > MAX_LINE_CHARS {
            " ..."
        } else {
            ""
        };
        format!("{:>4} | {shown}{ellipsis}", index + 1)
    };
    let count = to + 1 - from;
    if count <= REGION_MAX_LINES {
        let whole: Vec<String> = (from..=to).map(render).collect();
        let joined = whole.join("\n");
        if joined.len() <= REGION_MAX_BYTES {
            return joined;
        }
    }
    // Take lines from both ends in turn while they and the note still fit; the note's
    // length is reserved for the largest count it could name.
    let note = |hidden: usize| format!("     … {hidden} lines not shown");
    let (mut head, mut tail) = (Vec::new(), Vec::new());
    let mut bytes = note(count).len();
    let (mut front, mut back) = (from, to);
    for _ in 0..count {
        let take_front = head.len() <= tail.len();
        let line = render(if take_front { front } else { back });
        if head.len() + tail.len() + 2 > REGION_MAX_LINES
            || bytes + line.len() + 1 > REGION_MAX_BYTES
        {
            break;
        }
        bytes += line.len() + 1;
        if take_front {
            head.push(line);
            front += 1;
        } else {
            tail.push(line);
            back = back.saturating_sub(1);
        }
    }
    let hidden = count - head.len() - tail.len();
    if hidden > 0 {
        head.push(note(hidden));
    }
    head.extend(tail.into_iter().rev());
    head.join("\n")
}

/// Fold `input` for the tolerant match: Unicode spaces, quotes and dashes become their
/// ASCII form and trailing whitespace before each line break is dropped, with a map from
/// every folded byte offset back to the offset in `input`.
fn normalize_tolerant(input: &str) -> (String, Vec<usize>) {
    let mut chars: Vec<(char, usize)> = Vec::new();
    let mut iter = input.char_indices().peekable();
    while let Some((index, character)) = iter.next() {
        let mapped = if character == '\r' {
            // A CRLF or a lone CR collapses to one LF.
            if iter.peek().is_some_and(|&(_, next)| next == '\n') {
                iter.next();
            }
            '\n'
        } else if is_unicode_space(character) {
            ' '
        } else if matches!(character, '\u{2018}' | '\u{2019}') {
            '\''
        } else if matches!(character, '\u{201C}' | '\u{201D}' | '\u{201E}' | '\u{201F}') {
            '"'
        } else if matches!(
            character,
            '\u{2010}'
                | '\u{2011}'
                | '\u{2012}'
                | '\u{2013}'
                | '\u{2014}'
                | '\u{2015}'
                | '\u{2212}'
        ) {
            '-'
        } else {
            character
        };
        chars.push((mapped, index));
    }

    // Drop the whitespace run at the end of every line, in the folded text.
    let mut keep = vec![true; chars.len()];
    let mut trailing = true;
    for (position, (character, _)) in chars.iter().enumerate().rev() {
        if *character == '\n' {
            trailing = true;
        } else if character.is_whitespace() && trailing {
            keep[position] = false;
        } else {
            trailing = false;
        }
    }

    let mut out = String::with_capacity(input.len());
    let mut offsets: Vec<usize> = Vec::with_capacity(input.len() + 1);
    for (position, (character, origin)) in chars.iter().enumerate() {
        if !keep[position] {
            continue;
        }
        let start = out.len();
        out.push(*character);
        for _ in start..out.len() {
            offsets.push(*origin);
        }
    }
    offsets.push(input.len());
    (out, offsets)
}

fn is_unicode_space(character: char) -> bool {
    matches!(
        character,
        '\u{00A0}' | '\u{1680}' | '\u{2000}'..='\u{200A}' | '\u{202F}' | '\u{205F}' | '\u{3000}'
    ) || (character.is_whitespace() && !matches!(character, '\n' | '\r' | '\t' | ' '))
}

/// All non-overlapping occurrences of `needle`, as ascending byte offsets.
fn find_all(haystack: &str, needle: &str) -> Vec<usize> {
    if needle.is_empty() {
        return Vec::new();
    }
    let mut positions = Vec::new();
    let mut from = 0;
    while let Some(relative) = haystack[from..].find(needle) {
        let absolute = from + relative;
        positions.push(absolute);
        from = absolute + needle.len();
    }
    positions
}

fn strip_bom(text: &str) -> (&str, bool) {
    text.strip_prefix('\u{FEFF}')
        .map_or((text, false), |stripped| (stripped, true))
}

/// The file's dominant line ending, taken from its first line break.
fn detect_line_ending(text: &str) -> &'static str {
    let bytes = text.as_bytes();
    for (index, byte) in bytes.iter().enumerate() {
        match byte {
            b'\r' => {
                return if bytes.get(index + 1) == Some(&b'\n') {
                    "\r\n"
                } else {
                    "\r"
                };
            }
            b'\n' => return "\n",
            _ => {}
        }
    }
    "\n"
}

/// Byte positions in the original for each normalized byte boundary; CRLF
/// consumes two source bytes but one normalized byte.
fn normalized_with_offsets(text: &str) -> (String, Vec<usize>) {
    let mut out = Vec::with_capacity(text.len());
    let mut offsets = Vec::with_capacity(text.len() + 1);
    let bytes = text.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        offsets.push(index);
        if bytes[index] == b'\r' {
            out.push(b'\n');
            index += 1;
            if bytes.get(index) == Some(&b'\n') {
                index += 1;
            }
        } else {
            out.push(bytes[index]);
            index += 1;
        }
    }
    offsets.push(bytes.len());
    (
        String::from_utf8(out).expect("normalizing valid UTF-8 retains validity"),
        offsets,
    )
}

fn normalize_to_lf(text: &str) -> String {
    if !text.contains('\r') {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    let mut characters = text.chars().peekable();
    while let Some(character) = characters.next() {
        if character == '\r' {
            out.push('\n');
            if characters.peek() == Some(&'\n') {
                characters.next();
            }
        } else {
            out.push(character);
        }
    }
    out
}

fn restore_line_endings(text: &str, ending: &str) -> String {
    match ending {
        "\r\n" => text.replace('\n', "\r\n"),
        "\r" => text.replace('\n', "\r"),
        _ => text.to_string(),
    }
}

/// Bound text shown to the model to `max_bytes` bytes and `max_lines` lines, cutting on a
/// char boundary, with a trailer when anything was cut.
///
/// The same algorithm and trailer as `p1_workspace::bound_output`, which the native file
/// tools share: this crate may not depend on `p1-workspace`, and the component must print
/// exactly what the native tool prints. `p1-tool-edit`'s tests hold the two equal.
pub fn bound_output(text: &str, max_bytes: usize, max_lines: usize) -> String {
    let total_bytes = text.len();
    let mut end = total_bytes;
    let mut truncated = false;

    // A trailing newline does not create an extra line, matching how the file tools count.
    if max_lines > 0 {
        let mut newlines = 0usize;
        for (index, character) in text.char_indices() {
            if character == '\n' {
                newlines += 1;
                if newlines == max_lines {
                    let next_line = index + 1;
                    if next_line < total_bytes {
                        end = next_line;
                        truncated = true;
                    }
                    break;
                }
            }
        }
    }

    if max_bytes < end {
        let mut cut = max_bytes;
        while cut > 0 && !text.is_char_boundary(cut) {
            cut -= 1;
        }
        end = cut;
        truncated = true;
    }

    if !truncated {
        return text.to_string();
    }

    let mut out = String::with_capacity(end + 64);
    out.push_str(&text[..end]);
    // The trailer goes on its own line without a blank line when a line-cap cut already
    // ends the kept slice with a newline.
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out.push_str(&format!(
        "[output truncated: showing {end} of {total_bytes} bytes]"
    ));
    out
}

/// Whether `requested` leaves the workspace by its spelling alone: `..` that climbs above the
/// root, or an absolute path.
///
/// A component describes a call on the restricted path, with no capability, so it cannot
/// resolve symlinks or learn the root; an absolute path is therefore flagged, because the
/// guest cannot show it is inside. Confinement itself is enforced again by the host at
/// `execute` whatever this says. The native tool decides with the real resolution instead.
pub fn escapes_lexically(requested: &str) -> bool {
    if requested.starts_with('/') {
        return true;
    }
    let mut depth = 0usize;
    for component in requested.split('/') {
        match component {
            "" | "." => {}
            ".." => match depth.checked_sub(1) {
                Some(up) => depth = up,
                None => return true,
            },
            _ => depth += 1,
        }
    }
    false
}

/// `requested` with `.` and `..` collapsed and empty components dropped, `/`-separated; for a
/// relative request inside the root this is how the workspace service displays it.
pub fn lexical_normalize(requested: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    for component in requested.split('/') {
        match component {
            "" | "." => {}
            ".." => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out.join("/")
}

/// What a result describes (ADR-0059): a one-line summary and, for a successful edit, the
/// diff of the replaced text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResultSummary {
    pub summary: String,
    pub diff: Option<Diff>,
}

/// The replaced text of one edit call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diff {
    pub path: String,
    pub before: String,
    pub after: String,
}

/// Describe the result `content` of a call whose parsed input is `input` (`None` when the
/// input does not parse); `ok` is whether the result's status is ok.
pub fn describe_result(input: Option<EditInput>, ok: bool, content: &str) -> ResultSummary {
    let (true, Some(input)) = (ok, input) else {
        return ResultSummary {
            summary: content.lines().next().unwrap_or_default().to_string(),
            diff: None,
        };
    };
    // A no-op edit changed nothing: describe it as such, not as a zero-effect diff.
    if is_no_change(&input) {
        return ResultSummary {
            summary: content.lines().next().unwrap_or_default().to_string(),
            diff: None,
        };
    }
    let (before, after) = input.before_and_after();
    // The single form's count multiplies its lines; a list's entries each count once, since
    // the output names only their total.
    let replacements = if input.listed {
        1
    } else {
        parenthesized_count(content, "replacement").unwrap_or(1)
    };
    let lines = |pick: fn(&Replacement) -> &str| -> usize {
        input
            .edits
            .iter()
            .map(|entry| pick(entry).lines().count())
            .sum::<usize>()
            * replacements
    };
    ResultSummary {
        summary: format!(
            "+{} −{}",
            lines(|entry| &entry.new_string),
            lines(|entry| &entry.old_string)
        ),
        diff: Some(Diff {
            path: input.file_path,
            before,
            after,
        }),
    }
}

fn parenthesized_count(content: &str, unit: &str) -> Option<usize> {
    // Only the first line carries the count; an echoed tolerant region (ADR-0106) follows
    // it and holds its own parentheses, both the label's and the file's.
    let first = content.lines().next()?;
    let rest = &first[first.rfind('(')? + 1..];
    let digits_end = rest.find(|c: char| !c.is_ascii_digit())?;
    let count = rest[..digits_end].parse().ok()?;
    rest[digits_end..]
        .trim_start()
        .starts_with(unit)
        .then_some(count)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(old: &str, new: &str, replace_all: bool) -> EditInput {
        EditInput::single("f.txt", old, new, replace_all)
    }

    #[test]
    fn the_schema_is_the_declared_one() {
        let schema = input_schema();
        assert_eq!(schema["required"], serde_json::json!(["file_path"]));
        assert_eq!(schema["additionalProperties"], false);
        assert_eq!(schema["properties"].as_object().unwrap().len(), 5);
        let entry = &schema["properties"]["edits"]["items"];
        assert_eq!(
            entry["required"],
            serde_json::json!(["old_string", "new_string"])
        );
        assert_eq!(entry["additionalProperties"], false);
        assert_eq!(schema["properties"]["edits"]["minItems"], 1);
    }

    fn listed(entries: &[(&str, &str)]) -> EditInput {
        let edits: Vec<serde_json::Value> = entries
            .iter()
            .map(|(old, new)| serde_json::json!({"old_string": old, "new_string": new}))
            .collect();
        parse_json_input(
            "edit",
            &serde_json::json!({"file_path": "f.txt", "edits": edits}).to_string(),
        )
        .unwrap()
    }

    /// #706: the description documents the list form with an example that is itself a
    /// valid input.
    #[test]
    fn the_description_documents_the_list_form_with_a_valid_example() {
        assert!(DESCRIPTION.contains("`edits`"), "{DESCRIPTION}");
        let example = &DESCRIPTION[DESCRIPTION.find("Example: ").unwrap() + "Example: ".len()..];
        let example = example.lines().next().unwrap();
        let parsed = parse_json_input("edit", example).unwrap();
        assert!(parsed.listed);
        assert_eq!(parsed.edits.len(), 2);
    }

    /// #706: several replacements apply in one write, each matched against the file as it
    /// was before the call, in any entry order.
    #[test]
    fn a_list_applies_every_entry_against_the_original_text() {
        let body = b"fn alpha() {}\nfn beta() {}\nfn gamma() {}\n";
        let edited = edit_text(
            "f.txt",
            body,
            &listed(&[
                ("gamma", "GAMMA"),
                ("alpha", "ALPHA"),
                ("beta()", "beta(x)"),
            ]),
        )
        .unwrap();
        assert_eq!(
            edited.contents,
            "fn ALPHA() {}\nfn beta(x) {}\nfn GAMMA() {}\n"
        );
        assert_eq!(edited.replacements, 3);
        assert_eq!(edited.applied_region, None);
        // A later entry sees the original text, not an earlier entry's replacement.
        let chained = edit_text(
            "f.txt",
            b"one\n",
            &listed(&[("one", "two"), ("two", "three")]),
        );
        assert_eq!(
            chained,
            Err("edits[1] failed; no edit was applied. old_string was not found in f.txt.".into())
        );
        // Adjacent ranges do not overlap.
        let adjacent =
            edit_text("f.txt", b"abcd\n", &listed(&[("ab", "AB"), ("cd", "CD")])).unwrap();
        assert_eq!(adjacent.contents, "ABCD\n");
        // Line endings and the BOM are kept for every entry.
        let crlf = edit_text(
            "f.txt",
            "\u{FEFF}a\r\nb\r\nc\r\n".as_bytes(),
            &listed(&[("a", "A\nA"), ("c", "C")]),
        )
        .unwrap();
        assert_eq!(crlf.contents, "\u{FEFF}A\r\nA\r\nb\r\nC\r\n");
    }

    #[test]
    fn a_list_entry_that_is_missing_fails_the_whole_call_by_index() {
        let error = edit_text(
            "f.txt",
            b"the quick brown fox\n",
            &listed(&[("quick", "slow"), ("brown cat", "x")]),
        )
        .unwrap_err();
        assert!(
            error.starts_with(
                "edits[1] failed; no edit was applied. old_string was not found in f.txt.\nClosest matching region (around line 1):\n"
            ),
            "{error}"
        );
    }

    #[test]
    fn a_list_entry_that_matches_twice_fails_the_whole_call_by_index() {
        assert_eq!(
            edit_text("f.txt", b"one\ndup\ndup\n", &listed(&[("one", "1"), ("dup", "x")])),
            Err("edits[1] failed; no edit was applied. old_string occurs 2 times in f.txt; add context to make it unique or set replace_all.".into())
        );
        // An entry's own replace_all is honoured; its every match counts.
        let input = parse_json_input(
            "edit",
            r#"{"file_path": "f.txt", "edits": [{"old_string": "one", "new_string": "1"}, {"old_string": "dup", "new_string": "x", "replace_all": true}]}"#,
        )
        .unwrap();
        let edited = edit_text("f.txt", b"one\ndup\ndup\n", &input).unwrap();
        assert_eq!(edited.contents, "1\nx\nx\n");
        assert_eq!(edited.replacements, 3);
    }

    #[test]
    fn overlapping_list_entries_fail_the_whole_call_naming_both() {
        assert_eq!(
            edit_text(
                "f.txt",
                b"let value = 1;\n",
                &listed(&[("value = 1", "value = 2"), ("let value", "let v")])
            ),
            Err("edits[0] and edits[1] overlap in f.txt; no edit was applied. Merge them into one entry.".into())
        );
        // The same text twice is an overlap too.
        assert_eq!(
            edit_text("f.txt", b"once\n", &listed(&[("once", "a"), ("once", "b")])),
            Err("edits[0] and edits[1] overlap in f.txt; no edit was applied. Merge them into one entry.".into())
        );
    }

    #[test]
    fn list_inputs_are_validated() {
        let parse = |raw: &str| parse_json_input("edit", raw);
        assert_eq!(
            parse(r#"{"file_path": "a", "edits": []}"#),
            Err("Invalid input for edit: `edits` must not be empty".into())
        );
        assert_eq!(
            parse(
                r#"{"file_path": "a", "edits": [{"old_string": "x", "new_string": "y"}, {"old_string": "", "new_string": "y"}]}"#
            ),
            Err("Invalid input for edit: `edits[1].old_string` must not be empty".into())
        );
        let both = "Invalid input for edit: give either `edits` or `old_string`/`new_string`/`replace_all`, not both";
        assert_eq!(
            parse(
                r#"{"file_path": "a", "old_string": "x", "new_string": "y", "edits": [{"old_string": "x", "new_string": "y"}]}"#
            ),
            Err(both.into())
        );
        assert_eq!(
            parse(
                r#"{"file_path": "a", "replace_all": true, "edits": [{"old_string": "x", "new_string": "y"}]}"#
            ),
            Err(both.into())
        );
        assert!(
            parse(
                r#"{"file_path": "a", "edits": [{"old_string": "x", "new_string": "y", "z": 1}]}"#
            )
            .unwrap_err()
            .starts_with("Invalid input for edit: unknown field `z`")
        );
        // A spelled-out default is not a conflict.
        assert!(
            parse(r#"{"file_path": "a", "replace_all": false, "edits": [{"old_string": "x", "new_string": "y"}]}"#)
                .unwrap()
                .listed
        );
        // The single form's errors are serde's, with their position, as before.
        assert_eq!(
            parse(r#"{"file_path": "a"}"#),
            Err("Invalid input for edit: missing field `old_string` at line 1 column 18".into())
        );
        assert_eq!(
            parse(r#"{"file_path": "a", "old_string": "x"}"#),
            Err("Invalid input for edit: missing field `new_string` at line 1 column 37".into())
        );
        assert!(
            parse(r#"{"file_path": "a", "old_string": null, "new_string": "y"}"#)
                .unwrap_err()
                .starts_with("Invalid input for edit: invalid type: null, expected a string")
        );
        // The single form parses to one unlisted replacement, as before.
        assert_eq!(
            parse(r#"{"file_path": "a", "old_string": "x", "new_string": "y"}"#),
            Ok(EditInput::single("a", "x", "y", false))
        );
        // A list whose every entry changes nothing is a no-op; one real entry is not.
        let no_op = listed(&[("a", "a"), ("b", "b")]);
        assert!(is_no_change(&no_op));
        assert_eq!(
            no_change(&no_op),
            "No change: every entry's old_string and new_string are identical; f.txt was not modified."
        );
        assert!(!is_no_change(&listed(&[("a", "a"), ("b", "c")])));
    }

    /// A tolerant entry in a list echoes its region over the fully edited text.
    #[test]
    fn a_tolerant_list_entry_echoes_its_region() {
        let edited = edit_text(
            "f.txt",
            "head\nlet a = \u{201C}x\u{201D};\ntail\n".as_bytes(),
            &listed(&[
                ("tail", "TAIL"),
                ("let a = \"x\";", "let a = 1;"),
                ("head", "HEAD"),
            ]),
        )
        .unwrap();
        assert_eq!(edited.contents, "HEAD\nlet a = 1;\nTAIL\n");
        assert_eq!(
            edited.applied_region.as_deref(),
            Some("   1 | HEAD\n   2 | let a = 1;\n   3 | TAIL\n   4 | ")
        );
    }

    #[test]
    fn a_list_is_described_entry_by_entry() {
        let input = listed(&[("a\nb", "c"), ("d", "e\nf\ng")]);
        assert_eq!(
            input.before_and_after(),
            ("a\nb\n…\nd".to_string(), "c\n…\ne\nf\ng".to_string())
        );
        let described = describe_result(Some(input), true, "Edited f.txt (2 replacements).");
        assert_eq!(described.summary, "+4 −3");
        assert_eq!(
            described.diff,
            Some(Diff {
                path: "f.txt".into(),
                before: "a\nb\n…\nd".into(),
                after: "c\n…\ne\nf\ng".into(),
            })
        );
    }

    /// Issue #505: the model-facing text names the exact-first match, the tolerant fallback
    /// (ADR-0106) and the echoed region.
    #[test]
    fn the_description_names_the_tolerant_fallback() {
        let old = input_schema()["properties"]["old_string"]["description"]
            .as_str()
            .unwrap()
            .to_string();
        for text in [DESCRIPTION, old.as_str()] {
            assert!(text.contains("exactly first"), "{text}");
            assert!(text.contains("tolerant"), "{text}");
            assert!(text.contains("applied region"), "{text}");
            assert!(!text.contains("Replace an exact string"), "{text}");
        }
    }

    #[test]
    fn invalid_inputs_name_the_tool_and_the_reason() {
        assert_eq!(
            parse_json_input(
                "edit",
                r#"{"file_path":"a","old_string":"","new_string":"b"}"#
            ),
            Err("Invalid input for edit: `old_string` must not be empty".into())
        );
        // #458 unit 3: identical strings are a valid no-op, not an input error.
        let identical = parse_json_input(
            "EditFile",
            r#"{"file_path":"a","old_string":"x","new_string":"x"}"#,
        )
        .unwrap();
        assert!(is_no_change(&identical));
        let unknown = parse_json_input(
            "edit",
            r#"{"file_path":"a","old_string":"x","new_string":"y","z":1}"#,
        )
        .unwrap_err();
        assert!(
            unknown.starts_with("Invalid input for edit: unknown field `z`"),
            "{unknown}"
        );
        assert_eq!(
            text_input_error("edit"),
            "Invalid input for edit: expected a JSON object input, got freeform text"
        );
    }

    #[test]
    fn replaces_a_unique_match() {
        let edited = edit_text("f.txt", b"one\ntwo\nthree\n", &input("two", "TWO", false)).unwrap();
        assert_eq!(edited.contents, "one\nTWO\nthree\n");
        assert_eq!(edited.replacements, 1);
        // An exact match is silent: no applied region is echoed.
        assert_eq!(edited.applied_region, None);
        assert_eq!(
            edited_output("f.txt", 1, None),
            "Edited f.txt (1 replacement)."
        );
        assert_eq!(
            edited_output("f.txt", 3, None),
            "Edited f.txt (3 replacements)."
        );
    }

    /// ADR-0106: an exact match is still tried first and stays terse, so the fallback can
    /// never change what an exactly-matching call does.
    #[test]
    fn an_exact_unique_match_wins_over_the_tolerant_one() {
        let body = "let name = \u{201C}Iris\u{201D};\nlet name = \"Iris\";\n".as_bytes();
        let edited = edit_text(
            "c.txt",
            body,
            &input("let name = \"Iris\";", "let x = 1;", false),
        )
        .unwrap();
        assert_eq!(
            edited.contents,
            "let name = \u{201C}Iris\u{201D};\nlet x = 1;\n"
        );
        assert_eq!(edited.replacements, 1);
        assert_eq!(edited.applied_region, None);
    }

    /// Trailing whitespace is folded, the Unicode confusables iris folds (curly quotes
    /// against ASCII ones) are folded, and indentation is not.
    #[test]
    fn a_unique_tolerant_match_is_applied_and_echoes_its_region() {
        let whitespace = edit_text(
            "w.txt",
            b"fn main() {   \n    let x = 1;   \n}\n",
            &input(
                "fn main() {\n    let x = 1;\n}",
                "fn main() {\n    let x = 2;\n}",
                false,
            ),
        )
        .unwrap();
        assert_eq!(whitespace.contents, "fn main() {\n    let x = 2;\n}\n");
        assert_eq!(whitespace.replacements, 1);
        let region = whitespace.applied_region.as_deref().unwrap();
        assert!(region.contains("let x = 2;"), "{region}");
        assert!(region.contains("fn main()"), "context line: {region}");
        assert_eq!(
            edited_output(
                "w.txt",
                whitespace.replacements,
                whitespace.applied_region.as_deref()
            ),
            format!("Edited w.txt (1 replacement).\nApplied region (tolerant match):\n{region}")
        );

        let quotes = edit_text(
            "q.txt",
            "let name = \"Iris\";\n".as_bytes(),
            &input("\u{201C}Iris\u{201D}", "\"IRIS\"", false),
        )
        .unwrap();
        assert_eq!(quotes.contents, "let name = \"IRIS\";\n");
        assert_eq!(quotes.replacements, 1);
        assert!(quotes.applied_region.as_deref().unwrap().contains("IRIS"));

        // Indentation is not folded, so a differently indented needle is not found.
        let indented = edit_text(
            "w.txt",
            b"fn main() {\n    let x = 1;\n}\n",
            &input("fn main() {\nlet x = 1;\n}", "x", false),
        );
        assert!(
            indented
                .as_ref()
                .unwrap_err()
                .starts_with("old_string was not found in w.txt.\nClosest matching region"),
            "{indented:?}"
        );
    }

    /// The count is in the message for both passes: an ambiguous exact match and an
    /// ambiguous tolerant one are the same error.
    #[test]
    fn an_ambiguous_tolerant_match_names_the_count() {
        // ASCII quotes against curly ones: the exact pass finds nothing, the folded one twice.
        let tolerant = edit_text(
            "e.txt",
            "let a = \u{201C}x\u{201D};\nother\nlet b = \u{201C}x\u{201D};\n".as_bytes(),
            &input("\"x\";", "x", false),
        );
        assert_eq!(
            tolerant,
            Err("old_string occurs 2 times in e.txt; add context to make it unique or set replace_all.".into())
        );
        let exact = edit_text("e.txt", b"dup\ndup\n", &input("dup", "x", false));
        assert_eq!(
            exact,
            Err("old_string occurs 2 times in e.txt; add context to make it unique or set replace_all.".into())
        );
    }

    /// The not-found error keeps p1's first sentence and appends the line-numbered closest
    /// region, so the model can re-anchor without re-reading the file.
    #[test]
    fn not_found_shows_the_closest_region() {
        let body = b"the quick brown fox\njumps over\nthe lazy dog\n";
        let error =
            edit_text("p.txt", body, &input("the quick brown cat", "x", false)).unwrap_err();
        assert!(
            error.starts_with(
                "old_string was not found in p.txt.\nClosest matching region (around line 1):\n"
            ),
            "{error}"
        );
        assert!(error.contains("   1 | the quick brown fox"), "{error}");
        assert!(error.contains("   3 | the lazy dog"), "{error}");

        // Nothing resembling the needle: the first sentence alone.
        assert_eq!(
            edit_text("p.txt", body, &input("zzzz", "x", false)),
            Err("old_string was not found in p.txt.".into())
        );
    }

    /// A long line in the region is cut, as iris cuts it.
    #[test]
    fn the_closest_region_is_bounded() {
        let long = "x".repeat(500);
        let body = format!("needle here {long}\n");
        let error = edit_text(
            "p.txt",
            body.as_bytes(),
            &input("needle here short", "x", false),
        )
        .unwrap_err();
        // 200 characters a line in all, as iris counts: the 12-character prefix leaves 188.
        let shown = format!("   1 | needle here {} ...\n", "x".repeat(188));
        assert!(error.contains(&shown), "{error}");
        assert!(!error.contains(&"x".repeat(189)), "{error}");
    }

    /// #509 item 3: a 5,000-line tolerant replacement echoes both ends of its region and an
    /// elision note, within the region's own bound, never the output bound's cut.
    #[test]
    fn a_huge_tolerant_region_is_elided_inside_the_envelope() {
        let body: String = (1..=5_000).map(|n| format!("old line {n}   \n")).collect();
        let old: Vec<String> = (1..=5_000).map(|n| format!("old line {n}")).collect();
        let new: Vec<String> = (1..=5_000).map(|n| format!("new line {n}")).collect();
        let edited = edit_text(
            "big.txt",
            body.as_bytes(),
            &input(&old.join("\n"), &new.join("\n"), false),
        )
        .unwrap();
        let region = edited.applied_region.as_deref().unwrap();
        assert!(region.len() <= REGION_MAX_BYTES, "{} bytes", region.len());
        assert!(region.lines().count() <= REGION_MAX_LINES);
        assert!(region.starts_with("   1 | new line 1\n"), "{region}");
        assert!(
            region.ends_with("5000 | new line 5000\n5001 | "),
            "{region}"
        );
        let note = region
            .lines()
            .find(|line| line.starts_with("     … "))
            .expect("an elision note");
        let shown = region.lines().count() - 1;
        assert_eq!(note, format!("     … {} lines not shown", 5_001 - shown));
        let output = edited_output("big.txt", 1, Some(region));
        assert!(!output.contains("[output truncated"), "{output}");
        assert!(output.len() <= MAX_OUTPUT_BYTES && output.lines().count() < MAX_OUTPUT_LINES);

        // Long multi-byte lines hit the byte bound before the line bound.
        let wide: String = (1..=30)
            .map(|n| format!("{n} {}   \n", "é".repeat(300)))
            .collect();
        let old: String = (1..=30)
            .map(|n| format!("{n} {}\n", "é".repeat(300)))
            .collect();
        let new = old.replace('é', "è");
        let edited = edit_text("w.txt", wide.as_bytes(), &input(&old, &new, false)).unwrap();
        let region = edited.applied_region.unwrap();
        assert!(region.len() <= REGION_MAX_BYTES, "{} bytes", region.len());
        assert!(region.contains(" lines not shown"), "{region}");
    }

    #[test]
    fn replace_all_replaces_every_tolerant_occurrence() {
        let body = "let a = \u{201C}x\u{201D};\nother\nlet b = \u{201C}x\u{201D};\n".as_bytes();
        let edited = edit_text("e.txt", body, &input("\"x\";", "y;", true)).unwrap();
        assert_eq!(edited.contents, "let a = y;\nother\nlet b = y;\n");
        assert_eq!(edited.replacements, 2);
        // Only the first applied region is echoed, as iris echoes, over the fully edited text.
        let region = edited.applied_region.as_deref().unwrap();
        assert!(region.contains("   1 | let a = y;"), "{region}");
        assert!(region.contains("   3 | let b = y;"), "{region}");
    }

    /// The echoed region must not hide the count from the result summary (review H2).
    #[test]
    fn a_tolerant_replace_all_summary_counts_every_replacement() {
        let edit = input("\"x\"", "y", true);
        let edited = edit_text(
            "f.txt",
            "\u{201C}x\u{201D}\n\u{201C}x\u{201D}\n".as_bytes(),
            &edit,
        )
        .unwrap();
        let output = edited_output(
            "f.txt",
            edited.replacements,
            edited.applied_region.as_deref(),
        );
        assert!(output.contains("(tolerant match)"), "{output}");
        assert_eq!(describe_result(Some(edit), true, &output).summary, "+2 −2");
    }

    /// A CR-only file's region is numbered by line, without carriage returns (review H3).
    #[test]
    fn a_tolerant_region_in_a_cr_file_is_numbered_by_line() {
        let edited = edit_text(
            "r.txt",
            "a\r\u{201C}x\u{201D}\rb".as_bytes(),
            &input("\"x\"", "y", false),
        )
        .unwrap();
        assert_eq!(edited.contents, "a\ry\rb");
        assert_eq!(
            edited.applied_region.as_deref(),
            Some("   1 | a\n   2 | y\n   3 | b")
        );
    }

    #[test]
    fn refuses_missing_and_ambiguous_matches_and_non_utf8() {
        assert_eq!(
            edit_text("g.txt", b"alpha\n", &input("beta", "x", false)),
            Err("old_string was not found in g.txt.".into())
        );
        assert_eq!(
            edit_text("e.txt", b"dup\ndup\n", &input("dup", "x", false)),
            Err(
                "old_string occurs 2 times in e.txt; add context to make it unique or set replace_all."
                    .into()
            )
        );
        assert_eq!(
            edit_text("b.bin", &[0xff, 0xfe, b'a'], &input("a", "b", false)),
            Err("b.bin is not valid UTF-8.".into())
        );
    }

    #[test]
    fn replace_all_counts_every_occurrence() {
        let edited = edit_text("f.txt", b"dup\ndup\ndup\n", &input("dup", "x", true)).unwrap();
        assert_eq!(edited.contents, "x\nx\nx\n");
        assert_eq!(edited.replacements, 3);
    }

    #[test]
    fn keeps_crlf_lone_cr_a_missing_final_newline_and_a_bom() {
        let crlf = edit_text("f", b"one\r\ntwo\r\n", &input("one\ntwo", "1\n2", false)).unwrap();
        assert_eq!(crlf.contents, "1\r\n2\r\n");
        let cr = edit_text("f", b"one\rtwo\r", &input("two", "TWO", false)).unwrap();
        assert_eq!(cr.contents, "one\rTWO\r");
        let bare = edit_text("f", b"one\ntwo", &input("two", "TWO", false)).unwrap();
        assert_eq!(bare.contents, "one\nTWO");
        let bom = edit_text(
            "f",
            "\u{FEFF}alpha\n".as_bytes(),
            &input("alpha", "beta", false),
        )
        .unwrap();
        assert_eq!(bom.contents, "\u{FEFF}beta\n");
        // A CRLF old_string matches an LF file too: matching is on normalized text.
        let lf = edit_text("f", b"a\nb\n", &input("a\r\nb", "c", false)).unwrap();
        assert_eq!(lf.contents, "c\n");
    }

    #[test]
    fn mixed_endings_outside_replacement_remain_unchanged() {
        let edited = edit_text("f", b"a\r\nb\nc\r\n", &input("b", "B", false)).unwrap();
        assert_eq!(edited.contents, "a\r\nB\nc\r\n");
    }

    #[test]
    fn bounds_long_output_with_a_trailer() {
        assert_eq!(bound_output("short", 10, 10), "short");
        assert_eq!(
            bound_output("a\nb\nc\n", 100, 2),
            "a\nb\n[output truncated: showing 4 of 6 bytes]"
        );
        assert_eq!(
            bound_output("héllo", 2, 0),
            "h\n[output truncated: showing 1 of 6 bytes]"
        );
    }

    #[test]
    fn lexical_escape_and_normalization() {
        assert!(!escapes_lexically("src/a.rs"));
        assert!(!escapes_lexically("a/../b"));
        assert!(!escapes_lexically(""));
        assert!(escapes_lexically("../a.rs"));
        assert!(escapes_lexically("a/../../b"));
        assert!(escapes_lexically("/etc/passwd"));
        assert_eq!(lexical_normalize("./a//b/../c.txt"), "a/c.txt");
        assert_eq!(lexical_normalize("d.txt"), "d.txt");
    }

    #[test]
    fn describes_results() {
        let described = describe_result(
            Some(input("a\nb", "c", false)),
            true,
            "Edited f.txt (2 replacements).",
        );
        assert_eq!(described.summary, "+2 −4");
        assert_eq!(
            described.diff,
            Some(Diff {
                path: "f.txt".into(),
                before: "a\nb".into(),
                after: "c".into(),
            })
        );
        let failed = describe_result(Some(input("a", "b", false)), false, "first\nsecond");
        assert_eq!(failed.summary, "first");
        assert_eq!(failed.diff, None);
        // #458 unit 3: a no-op edit is described as no change, not as a zero-effect diff.
        let no_op = describe_result(
            Some(input("a", "a", false)),
            true,
            "No change: old_string and new_string are identical; f.txt was not modified.",
        );
        assert_eq!(
            no_op.summary,
            "No change: old_string and new_string are identical; f.txt was not modified."
        );
        assert_eq!(no_op.diff, None);
        assert_eq!(describe_result(None, true, "").summary, "");
    }
}
