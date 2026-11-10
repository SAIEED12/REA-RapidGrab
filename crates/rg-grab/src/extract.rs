//! Tolerant URL extraction from an HTML page.
//!
//! The grabber deliberately avoids a full HTML parser: it needs every
//! `href`/`src`-style attribute plus absolute URLs embedded in inline scripts
//! (`"playAddr":"https:\/\/…"`), and it must never fail on malformed markup.
//! The scanner therefore walks attributes and absolute URLs, and callers
//! deduplicate/validate the results.

const ATTRS: [&str; 9] = [
    "href",
    "src",
    "data-src",
    "data-href",
    "data-url",
    "data-video",
    "poster",
    "content",
    "cite",
];

/// All candidate URLs on the page, in document order, decoded and trimmed.
pub fn extract_urls(html: &str) -> Vec<String> {
    // JSON escapes slashes in URLs (`https:\/\/cdn…`); normalise before the
    // raw scan so script-embedded media is visible. Length is preserved, so
    // the lowercase copy remains index-compatible.
    let normalized = html.replace("\\/", "/");
    let lower = normalized.to_ascii_lowercase();

    let mut out = Vec::new();
    for attr in ATTRS {
        scan_attribute(&normalized, &lower, attr, &mut out);
    }
    scan_absolute_urls(&normalized, &lower, &mut out);

    let mut seen = std::collections::HashSet::new();
    out.retain(|url| seen.insert(url.clone()));
    out
}

fn scan_attribute(html: &str, lower: &str, attr: &str, out: &mut Vec<String>) {
    let mut search = 0usize;
    while let Some(offset) = lower[search..].find(attr) {
        let start = search + offset;
        search = start + attr.len();

        // The attribute name must start at a boundary: `<a href=` or spaced.
        let boundary = start == 0
            || html[..start]
                .chars()
                .next_back()
                .is_some_and(|c| c.is_ascii_whitespace() || c == '<' || c == '"' || c == '\'');
        if !boundary {
            continue;
        }

        let rest = &html[search..];
        let mut chars = rest.char_indices().peekable();
        // Skip whitespace before `=`.
        while chars.peek().is_some_and(|(_, c)| c.is_whitespace()) {
            chars.next();
        }
        match chars.peek() {
            Some((_, '=')) => {
                chars.next();
            }
            _ => continue,
        }
        while chars.peek().is_some_and(|(_, c)| c.is_whitespace()) {
            chars.next();
        }
        let Some((_, quote @ ('"' | '\''))) = chars.next() else {
            // Unquoted attribute value: read to whitespace or `>`.
            let value: String = rest[chars.peek().map(|(i, _)| *i).unwrap_or(rest.len())..]
                .chars()
                .take_while(|c| !c.is_whitespace() && *c != '>')
                .collect();
            push_attr_value(attr, &value, out);
            continue;
        };
        let value_start = chars.next().map(|(i, _)| i).unwrap_or(rest.len());
        let value_end = rest[value_start..]
            .find(quote)
            .map(|i| value_start + i)
            .unwrap_or(rest.len());
        push_attr_value(attr, &rest[value_start..value_end], out);
    }
}

/// Absolute `http(s)` URLs anywhere in the text (inline JSON, CSS, scripts).
fn scan_absolute_urls(html: &str, lower: &str, out: &mut Vec<String>) {
    let mut search = 0usize;
    while let Some(offset) = lower[search..].find("http") {
        let start = search + offset;
        search = start + 4;
        // Only `http://` or `https://`, and not in the middle of a word.
        let boundary = start == 0
            || lower[..start]
                .chars()
                .next_back()
                .is_some_and(|c| {
                    c.is_ascii_whitespace()
                        || matches!(c, '"' | '\'' | '(' | '[' | '{' | '=' | ':' | ',' | ';' | '<' | '>' | '`')
                });
        if !boundary || !(lower[start..].starts_with("http://") || lower[start..].starts_with("https://")) {
            continue;
        }
        let candidate: String = html[start..]
            .chars()
            .take_while(|c| {
                !c.is_whitespace()
                    && !matches!(c, '"' | '\'' | '<' | '>' | '\\' | ')' | ']' | '}' | ',' | '`')
            })
            .collect();
        push_absolute(&candidate, out);
    }
}

/// Attribute values may be relative (`/files/a.zip`, `e.jpg`), so they are
/// kept as-is; the caller resolves them against the page URL.
fn push_attr_value(attr: &str, raw: &str, out: &mut Vec<String>) {
    let value = decode_entities(raw.trim());
    if value.is_empty() || value.chars().any(char::is_whitespace) {
        return;
    }
    let lower = value.to_ascii_lowercase();
    if value.starts_with('#')
        || ["javascript:", "mailto:", "tel:", "data:", "blob:", "about:"]
            .iter()
            .any(|scheme| lower.starts_with(scheme))
    {
        return;
    }
    // Meta `content` is only useful for absolute og:video/og:image values;
    // other meta content (`summary_large_image`, `IE=edge`, …) is not a URL.
    if attr == "content"
        && !(lower.starts_with("http://")
            || lower.starts_with("https://")
            || value.starts_with("//"))
    {
        return;
    }
    out.push(value);
}

/// The free-text scan only accepts fully absolute URLs.
fn push_absolute(raw: &str, out: &mut Vec<String>) {
    let value = decode_entities(raw.trim());
    if value.starts_with("http://") || value.starts_with("https://") {
        out.push(value);
    }
}

fn decode_entities(value: &str) -> String {
    value
        .replace("&amp;", "&")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&#x2F;", "/")
        .replace("&#x2f;", "/")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
}

#[cfg(test)]
mod tests {
    use super::extract_urls;

    #[test]
    fn finds_common_attributes() {
        let html = r#"
            <a href="/files/a.zip">A</a>
            <a href='https://cdn.example.com/b.mp4?v=1&amp;x=2'>B</a>
            <video src="/media/c.webm"></video>
            <source data-src="//img.example.com/d.png">
            <img src="e.jpg">
        "#;
        let urls = extract_urls(html);
        assert!(urls.contains(&"/files/a.zip".to_string()));
        assert!(urls.contains(&"https://cdn.example.com/b.mp4?v=1&x=2".to_string()));
        assert!(urls.contains(&"/media/c.webm".to_string()));
        assert!(urls.contains(&"//img.example.com/d.png".to_string()));
        assert!(urls.contains(&"e.jpg".to_string()));
    }

    #[test]
    fn finds_absolute_urls_in_inline_json() {
        let html = r#"<script>var cfg = {"playAddr":"https:\/\/cdn.example.com\/v\/clip.mp4","poster":"https:\/\/cdn.example.com\/p.jpg"};</script>"#;
        let urls = extract_urls(html);
        assert!(urls.contains(&"https://cdn.example.com/v/clip.mp4".to_string()));
        assert!(urls.contains(&"https://cdn.example.com/p.jpg".to_string()));
    }

    #[test]
    fn deduplicates_and_ignores_relative_bare_text() {
        let html = r#"<a href="https://ex.com/a">1</a><a href="https://ex.com/a">2</a>"#;
        assert_eq!(extract_urls(html), vec!["https://ex.com/a".to_string()]);
    }

    #[test]
    fn does_not_treat_attribute_names_as_urls() {
        let html = r#"<div data-href="https://ex.com/x" class="href">"#;
        assert_eq!(extract_urls(html), vec!["https://ex.com/x".to_string()]);
    }
}
