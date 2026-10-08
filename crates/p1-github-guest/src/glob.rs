use crate::Error;
use regex::Regex;

/// Translate only the advertised glob grammar, never accept a user regex.
pub(crate) fn compile(pattern: &str) -> Result<Regex, Error> {
    let chars: Vec<_> = pattern.chars().collect();
    let mut out = String::from("^");
    let mut i = 0;
    let mut brace = false;
    while i < chars.len() {
        match chars[i] {
            '*' if chars.get(i + 1) == Some(&'*') => {
                i += 1;
                if chars.get(i + 1) == Some(&'/') {
                    out.push_str("(?:.*/)?");
                    i += 1;
                } else {
                    out.push_str(".*");
                }
            }
            '*' => out.push_str("[^/]*"),
            '?' => out.push_str("[^/]"),
            '{' if !brace => {
                out.push_str("(?:");
                brace = true;
            }
            ',' if brace => out.push('|'),
            '}' if brace => {
                out.push(')');
                brace = false;
            }
            '{' | '}' => return Err("invalid or nested glob alternation".into()),
            '[' => {
                // Negated classes exclude slash too. Positive classes are intersected
                // with the non-slash class, so '?' and classes never cross segments.
                out.push_str("[[");
                i += 1;
                if chars.get(i) == Some(&'!') {
                    out.push('^');
                    i += 1;
                }
                let start = i;
                while i < chars.len() && chars[i] != ']' {
                    if matches!(chars[i], '[' | '\\' | '&') {
                        return Err("unsupported glob character class".into());
                    }
                    out.push(chars[i]);
                    i += 1;
                }
                if i == chars.len() || i == start {
                    return Err("unclosed or empty glob character class".into());
                }
                out.push_str("]&&[^/]]");
            }
            '\\' => return Err("backslash escapes are not supported in globs".into()),
            c => out.push_str(&regex::escape(&c.to_string())),
        }
        i += 1;
    }
    if brace {
        return Err("unclosed glob alternation".into());
    }
    out.push('$');
    Regex::new(&out).map_err(|_| "invalid glob pattern".into())
}
