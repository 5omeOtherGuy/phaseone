//! Deliberate two-field subset, not a general YAML parser (owner 2026-10-10).

pub(super) fn read_front_matter(text: &str) -> Result<(Option<String>, String), String> {
    let lines: Vec<_> = text.lines().collect();
    let mut name = None;
    let mut description = None;
    let mut index = 0;
    while index < lines.len() {
        let line = lines[index];
        index += 1;
        if line.trim().is_empty() || line.trim_start().starts_with('#') {
            continue;
        }
        // Unknown fields and their nested maps do not belong to this reader.
        if line.starts_with(char::is_whitespace) {
            continue;
        }
        let Some((key, raw)) = line.split_once(':') else {
            return Err("invalid front matter field".into());
        };
        let destination = match key.trim() {
            "name" => &mut name,
            "description" => &mut description,
            _ => continue,
        };
        if destination.is_some() {
            return Err(format!("duplicate skill {}", key.trim()));
        }
        let raw = raw.trim();
        let value = if matches!(raw, ">" | ">-" | "|" | "|-") {
            let start = index;
            while index < lines.len()
                && (lines[index].trim().is_empty() || lines[index].starts_with(' '))
            {
                index += 1;
            }
            let block = &lines[start..index];
            let indent = block
                .iter()
                .filter(|line| !line.trim().is_empty())
                .map(|line| line.len() - line.trim_start_matches(' ').len())
                .min()
                .unwrap_or(0);
            let mut value = String::new();
            for (position, line) in block.iter().enumerate() {
                let line = if line.trim().is_empty() {
                    ""
                } else {
                    &line[indent..]
                };
                value.push_str(line);
                let next = block.get(position + 1);
                if raw.starts_with('>') && !line.is_empty() && !line.starts_with(' ') {
                    if next.is_some_and(|next| next.trim().is_empty()) {
                        // The blank line supplies the paragraph break itself.
                    } else if next.is_some_and(|next| {
                        next.len() - next.trim_start_matches(' ').len() == indent
                    }) {
                        value.push(' ');
                    } else {
                        value.push('\n');
                    }
                } else {
                    value.push('\n');
                }
            }
            let stripped = value.trim_end_matches('\n');
            if raw.ends_with('-') || stripped.is_empty() {
                stripped.to_owned()
            } else {
                format!("{stripped}\n")
            }
        } else if raw.starts_with('"') {
            serde_json::from_str::<String>(raw).map_err(|_| "invalid double-quoted skill value")?
        } else if raw.starts_with('\'') {
            if raw.len() < 2 || !raw.ends_with('\'') {
                return Err("unclosed single-quoted skill value".into());
            }
            let inner = &raw[1..raw.len() - 1];
            let mut chars = inner.chars().peekable();
            let mut value = String::new();
            while let Some(character) = chars.next() {
                if character == '\'' && chars.next() != Some('\'') {
                    return Err("invalid single-quoted skill value".into());
                }
                value.push(character);
            }
            value
        } else {
            if raw.starts_with(['[', '{', '&', '*', '!', '>', '|']) {
                return Err("unsupported skill value; expected text".into());
            }
            raw.split(" #")
                .next()
                .unwrap_or_default()
                .trim_end()
                .to_owned()
        };
        // Clip markers append a presentation newline, not part of the skill id.
        *destination = Some(if key.trim() == "name" && matches!(raw, ">" | "|") {
            value.trim_end_matches('\n').to_owned()
        } else {
            value
        });
    }
    Ok((name, description.ok_or("missing skill description")?))
}
