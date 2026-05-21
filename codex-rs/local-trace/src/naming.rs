use chrono::Local;

pub const MAX_LABEL_BYTES: usize = 96;

#[derive(Debug, Clone)]
pub struct TraceNamer {
    next_idx: u64,
}

impl Default for TraceNamer {
    fn default() -> Self {
        Self::new()
    }
}

impl TraceNamer {
    pub fn new() -> Self {
        Self { next_idx: 1 }
    }

    pub fn next(&mut self, label: &str) -> String {
        let prefix = Local::now().format("%Y%m%d-%H%M%S").to_string();
        self.next_with_timestamp(&prefix, label)
    }

    pub fn next_with_timestamp(&mut self, timestamp: &str, label: &str) -> String {
        let idx = self.next_idx;
        self.next_idx += 1;
        format!("{timestamp}-{idx:04}-{}", sanitize_label(label, "internal"))
    }
}

pub fn turn_slug(prompt: &str) -> String {
    let words = prompt
        .split_whitespace()
        .filter_map(sanitize_word)
        .take(4)
        .collect::<Vec<_>>();
    if words.is_empty() {
        "turn".to_string()
    } else {
        words.join("-")
    }
}

pub fn internal_label(label: Option<&str>) -> String {
    label
        .map(|label| sanitize_label(label, "internal"))
        .filter(|label| !label.is_empty())
        .unwrap_or_else(|| "internal".to_string())
}

pub fn tool_label(tool_name: &str) -> String {
    cap_label(&sanitize_label(tool_name, "tool"), 64)
}

pub fn sanitize_label(label: &str, fallback: &str) -> String {
    let sanitized = label
        .split_whitespace()
        .filter_map(sanitize_word)
        .collect::<Vec<_>>()
        .join("-");
    let sanitized = if sanitized.is_empty() {
        fallback.to_string()
    } else {
        sanitized
    };
    cap_label(&sanitized, MAX_LABEL_BYTES)
}

fn sanitize_word(word: &str) -> Option<String> {
    let sanitized = word
        .chars()
        .filter_map(|ch| {
            if ch.is_ascii_alphanumeric() {
                Some(ch.to_ascii_lowercase())
            } else if matches!(ch, '-' | '_' | '.') {
                Some(ch)
            } else {
                None
            }
        })
        .collect::<String>()
        .trim_matches(|ch| matches!(ch, '-' | '_' | '.'))
        .to_string();
    if sanitized.is_empty() {
        None
    } else {
        Some(sanitized)
    }
}

fn cap_label(label: &str, max_bytes: usize) -> String {
    if label.len() <= max_bytes {
        return label.to_string();
    }
    label[..max_bytes].trim_matches('-').to_string()
}
