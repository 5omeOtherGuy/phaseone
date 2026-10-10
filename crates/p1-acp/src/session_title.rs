//! Session-local title selection; no model call or persisted title is invented.

#[derive(Default)]
pub(crate) struct SessionTitle {
    opening: Option<String>,
    published: Option<String>,
}

impl SessionTitle {
    pub(crate) fn prompt(&mut self, text: &str) {
        if self.opening.is_none() {
            let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
            let mut title: String = text.chars().take(80).collect();
            // Prefer whole opening words, unless the first word alone exceeds the cap.
            if text.chars().nth(80).is_some_and(|c| !c.is_whitespace())
                && let Some(end) = title.rfind(' ')
            {
                title.truncate(end);
            }
            title.truncate(title.trim_end().len());
            if title.is_empty() {
                title = "Untitled session".to_string();
            }
            self.opening = Some(title);
        }
    }

    pub(crate) fn changed(&mut self, source: Option<String>) -> Option<String> {
        let title = source.or_else(|| self.opening.clone())?;
        if self.published.as_ref() == Some(&title) {
            return None;
        }
        self.published = Some(title.clone());
        Some(title)
    }
}
