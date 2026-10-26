//! A user's email signature: the rich text the signature editor saves.

use ammonia::Builder;
use std::borrow::Cow;
use std::sync::LazyLock;

/// The editor's formatting on top of ammonia's safe defaults: inline styles
/// and classes, link targets, and pictures (also pasted as `data:` images).
static CLEANER: LazyLock<Builder<'static>> = LazyLock::new(|| {
    let mut builder = Builder::default();
    builder
        .add_tags(["font"])
        .add_tag_attributes("font", ["color", "face", "size"])
        .add_generic_attributes(["style", "class"])
        .add_tag_attributes("a", ["target", "rel"])
        .link_rel(None)
        .add_url_schemes(["data"])
        .attribute_filter(|element, _attribute, value| {
            // Pictures may be inline data; links may not.
            let data = value.trim_start().to_ascii_lowercase().starts_with("data:");
            (element == "img" || !data).then_some(Cow::Borrowed(value))
        });
    builder
});

/// Visible text of `html`, ignoring tags.
fn has_content(html: &str) -> bool {
    let mut in_tag = false;
    let mut text = String::new();
    for c in html.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => text.push(c),
            _ => {}
        }
    }
    let text = text.replace("&nbsp;", " ").replace('\u{a0}', " ");
    !text.trim().is_empty() || html.to_ascii_lowercase().contains("<img")
}

/// The signature as stored: scripts, handlers and unsafe links removed, and
/// empty when it shows nothing (`sanitizeEmailSignature`).
pub fn sanitize(html: &str) -> String {
    let cleaned = CLEANER.clean(html).to_string();
    let cleaned = cleaned.trim();
    if has_content(cleaned) {
        cleaned.to_string()
    } else {
        String::new()
    }
}

#[cfg(test)]
mod tests {
    use super::sanitize;

    #[test]
    fn keeps_formatting_and_drops_scripts() {
        let html = r#"<p style="color: red"><b>Rita</b> <a href="https://acme.test" target="_blank" rel="noreferrer">site</a></p><script>alert(1)</script><img src="x" onerror="alert(1)">"#;
        let clean = sanitize(html);
        assert!(clean.contains(r#"<p style="color: red">"#), "{clean}");
        assert!(clean.contains(r#"target="_blank""#), "{clean}");
        assert!(!clean.contains("script"), "{clean}");
        assert!(!clean.contains("onerror"), "{clean}");
    }

    #[test]
    fn drops_data_links_and_keeps_data_pictures() {
        let clean =
            sanitize(r#"<a href="data:text/html,x">a</a><img src="data:image/png;base64,AAAA">"#);
        assert!(!clean.contains("text/html"), "{clean}");
        assert!(clean.contains("data:image/png"), "{clean}");
    }

    #[test]
    fn empty_rich_text_is_empty() {
        assert_eq!(sanitize("<p><br></p>"), "");
        assert_eq!(sanitize("  <p>&nbsp;</p> "), "");
        assert_eq!(sanitize("<script>x</script>"), "");
    }
}
