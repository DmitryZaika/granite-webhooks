//! The HTML and plain-text bodies of an email an employee sends.
//!
//! They hold the
//! composer's note (HTML from the editor, or plain text), an optional
//! forwarded-message block, the sender's signature and the soft opt-out line.
//! Port of the Remix app's `buildOutgoingEmailBodies`.

use regex::{Captures, Regex};
use std::sync::LazyLock;

pub const SOFT_OPT_OUT_LINE: &str =
    "If you'd rather not receive further emails from me, just reply and let me know.";

const SIGNATURE_STYLE: &str = "font-family: Georgia, 'Times New Roman', Times, serif; font-size: 14px; line-height: 1.35; color: #222222;";

static HTML_TAG: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?is)</?[a-z].*>").unwrap());
static ANY_TAG: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"<[^>]*>").unwrap());
static FORWARDED_BLOCK: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(?:^|\n)(-{5,}\s*Forwarded message\s*-{5,}[\s\S]*)$").unwrap()
});
static STYLE_OR_SCRIPT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?is)<style.*?</style>|<script.*?</script>").unwrap());
static STYLE_BLOCK: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?is)<style.*?</style>").unwrap());
static BR: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)<br\s*/?>").unwrap());
static BLOCK_END: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)</(?:p|div)>").unwrap());
static MANY_NEWLINES: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\n{3,}").unwrap());
static HTML_ANCHOR: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?is)<a\b[^>]*\bhref\s*=\s*(?:"([^"]*)"|'([^']*)'|([^\s>]+))[^>]*>(.*?)</a>"#)
        .unwrap()
});
static ENTITY: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"&(?:amp|lt|gt|quot|#39|nbsp|#\d+|#x[\da-fA-F]+);").unwrap());
static WHITESPACE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\s+").unwrap());
static EXISTING_ANCHOR: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?is)<a\b.*?</a>").unwrap());
static ANCHOR_OPEN: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)<a\s+([^>]*)>").unwrap());
static TARGET_ATTR: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)\btarget=").unwrap());
static INLINE_URL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(?im)(^|[\s>(])(https?://[^\s<>"']*[^\s<>"').,;:])"#).unwrap());
static SIGNATURE_URL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(?im)(^|[\s>])(https?://[^\s<>"']*[^\s<>"').,;:])"#).unwrap());
static MARKDOWN_LINK: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\[([^\]]+)\]\((https?://[^\s)]+|mailto:[^)]+)\)").unwrap());
static PLAIN_URL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(?i)\bhttps?://[^\s<>"')]*[^\s<>"').,;:]"#).unwrap());
static BOLD: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\*([^*\n]+)\*").unwrap());
static BULLET: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[-*]\s+(.+)$").unwrap());
static GREETING: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\b(Hi|Hello|Dear|This|Thank|Location:)\b").unwrap());
static CSS_NOISE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)(@media|!important|a:link|a:visited|\{|\})").unwrap());
static P_OPEN: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)<p(\s[^>]*)?>").unwrap());
static IMG_OPEN: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)<img(\s[^>]*)?>").unwrap());
static STYLE_ATTR: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"(?i)style=""#).unwrap());
static MARGIN: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)margin\s*:").unwrap());
static MAX_WIDTH: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)max-width").unwrap());
static LEADING_DASH: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s*(?:<[^>]+>\s*)*(?:--|—)").unwrap());

/// The two alternatives of an outgoing message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutgoingBodies {
    pub text: String,
    pub html: String,
}

pub fn outgoing_bodies(body: &str, signature: Option<&str>) -> OutgoingBodies {
    let trimmed = body.trim();
    // A forwarded block after an HTML note is plain text; inside an HTML body
    // its newlines would collapse into one line.
    let (note, forwarded) = split_forwarded_block(trimmed);
    let note = note.trim();
    let note_is_html = is_html_markup(note);
    let plain_note = if note_is_html {
        html_to_plain_text(note)
    } else {
        note.to_string()
    };
    let plain_body = match &forwarded {
        Some(forwarded) => [plain_note.as_str(), forwarded.as_str()]
            .iter()
            .filter(|part| !part.is_empty())
            .copied()
            .collect::<Vec<_>>()
            .join("\n\n"),
        None => plain_note,
    };
    let with_signature = append_signature(&plain_body, signature);
    let note_html = if note_is_html {
        sanitize_body(note)
    } else {
        sanitize_body(&plain_email_to_html(note))
    };
    let forwarded_html = forwarded
        .map(|forwarded| format!("<br>{}", sanitize_body(&plain_email_to_html(&forwarded))))
        .unwrap_or_default();
    let signature_html = signature_html(signature);
    let html = format!(
        "<html>\n    <body>\n      <div style=\"font-family: Arial, Helvetica, sans-serif; font-size: 14px;\">{note_html}{forwarded_html}</div>\n      {signature_html}\n      <div style=\"margin-top: 24px; font-size: 12px; color: #666666;\">\n        {SOFT_OPT_OUT_LINE}\n      </div>\n    </body>\n  </html>"
    );
    OutgoingBodies {
        text: format!("{with_signature}\n\n{SOFT_OPT_OUT_LINE}"),
        html,
    }
}

/// A system notice: the HTML as given, and its text without tags.
pub fn transactional_bodies(html: &str) -> OutgoingBodies {
    OutgoingBodies {
        text: html_to_plain_text(html),
        html: html.to_string(),
    }
}

pub fn is_html_markup(value: &str) -> bool {
    HTML_TAG.is_match(value)
}

/// `true` for a body with no text: empty, or only tags (`<p><br></p>`).
pub fn is_empty_rich_text(html: &str) -> bool {
    ANY_TAG.replace_all(html, "").trim().is_empty()
}

/// The sender's note and the trailing forwarded-message block, if any.
fn split_forwarded_block(body: &str) -> (String, Option<String>) {
    let normalized = body.replace("\r\n", "\n");
    match FORWARDED_BLOCK.captures(&normalized) {
        Some(captures) => {
            let start = captures.get(0).map_or(0, |m| m.start());
            let block = captures.get(1).map_or("", |m| m.as_str()).trim();
            (
                normalized[..start].trim_end().to_string(),
                Some(block.to_string()),
            )
        }
        None => (body.to_string(), None),
    }
}

pub fn html_to_plain_text(html: &str) -> String {
    let visible = STYLE_OR_SCRIPT.replace_all(html, "");
    let expanded = HTML_ANCHOR.replace_all(&visible, |captures: &Captures| {
        let href = [1, 2, 3]
            .iter()
            .find_map(|index| captures.get(*index).map(|m| m.as_str()))
            .unwrap_or("")
            .trim()
            .to_string();
        let label = ANY_TAG.replace_all(&captures[4], "");
        let label = WHITESPACE.replace_all(&label, " ").trim().to_string();
        if href.is_empty() {
            label
        } else if label.is_empty() || label == href {
            href
        } else {
            format!("{label} ({href})")
        }
    });
    let with_breaks = BR.replace_all(&expanded, "\n");
    let with_blocks = BLOCK_END.replace_all(&with_breaks, "\n");
    let text = ANY_TAG.replace_all(&with_blocks, "");
    let text = MANY_NEWLINES.replace_all(&text, "\n\n");
    decode_entities(text.trim())
}

fn decode_entities(text: &str) -> String {
    ENTITY
        .replace_all(text, |captures: &Captures| {
            let entity = &captures[0];
            match entity {
                "&amp;" => "&".to_string(),
                "&lt;" => "<".to_string(),
                "&gt;" => ">".to_string(),
                "&quot;" => "\"".to_string(),
                "&#39;" => "'".to_string(),
                "&nbsp;" => " ".to_string(),
                _ => {
                    let code = entity.strip_prefix("&#x").map_or_else(
                        || entity[2..entity.len() - 1].parse::<u32>().ok(),
                        |hex| u32::from_str_radix(&hex[..hex.len() - 1], 16).ok(),
                    );
                    code.and_then(char::from_u32)
                        .map_or_else(|| entity.to_string(), String::from)
                }
            }
        })
        .into_owned()
}

fn signature_plain_text(signature: Option<&str>) -> String {
    let raw = signature.unwrap_or("").trim();
    if raw.is_empty() || !is_html_markup(raw) {
        return raw.to_string();
    }
    html_to_plain_text(raw).trim().to_string()
}

fn append_signature(body: &str, signature: Option<&str>) -> String {
    let body = body.trim();
    let signature = signature_plain_text(signature);
    if signature.is_empty() || body.contains(&signature) {
        return body.to_string();
    }
    if signature.starts_with("--") {
        format!("{body}\n\n{signature}")
    } else {
        format!("{body}\n\n—\n{signature}")
    }
}

pub fn escape_html(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

fn escape_attribute(value: &str) -> String {
    escape_html(value).replace('`', "&#96;")
}

fn normalize_url(url: &str) -> String {
    url.replace("&amp;", "&").replace("&#38;", "&")
}

fn anchor(url: &str, label: &str) -> String {
    format!(
        "<a href=\"{}\" target=\"_blank\" rel=\"noreferrer\">{label}</a>",
        escape_attribute(url)
    )
}

fn sanitizer() -> ammonia::Builder<'static> {
    let mut builder = ammonia::Builder::default();
    builder
        .add_tags(["span", "div", "img", "u", "s"])
        .add_generic_attributes(["style", "class"])
        .add_tag_attributes("a", ["href", "target"])
        .add_tag_attributes("img", ["src", "alt", "width", "height"])
        .link_rel(Some("noreferrer"));
    builder
}

static SANITIZER: LazyLock<ammonia::Builder<'static>> = LazyLock::new(sanitizer);

/// Sanitizes the note's HTML, opens its links in a new tab and turns bare
/// URLs into links.
fn sanitize_body(html: &str) -> String {
    let sanitized = SANITIZER.clean(html).to_string();
    let with_targets = ANCHOR_OPEN.replace_all(&sanitized, |captures: &Captures| {
        if TARGET_ATTR.is_match(&captures[1]) {
            captures[0].to_string()
        } else {
            format!("<a target=\"_blank\" {}>", &captures[1])
        }
    });
    linkify_outside_anchors(&with_targets, &INLINE_URL, |prefix, url| {
        format!(
            "{prefix}<a href=\"{}\" target=\"_blank\" rel=\"noreferrer\">{}</a>",
            escape_attribute(&normalize_url(url)),
            escape_html(url)
        )
    })
}

/// Replaces URL matches of `pattern` (prefix in group 1, URL in group 2),
/// leaving existing `<a>` elements untouched.
fn linkify_outside_anchors(
    html: &str,
    pattern: &Regex,
    link: impl Fn(&str, &str) -> String,
) -> String {
    let mut output = String::with_capacity(html.len());
    let mut last = 0;
    for existing in EXISTING_ANCHOR.find_iter(html) {
        let before = &html[last..existing.start()];
        output.push_str(&pattern.replace_all(before, |c: &Captures| link(&c[1], &c[2])));
        output.push_str(existing.as_str());
        last = existing.end();
    }
    output.push_str(&pattern.replace_all(&html[last..], |c: &Captures| link(&c[1], &c[2])));
    output
}

/// Plain text (links, `*bold*`) of one line as HTML.
fn format_inline(value: &str) -> String {
    let mut replacements = Vec::new();
    let with_markers = MARKDOWN_LINK.replace_all(value, |captures: &Captures| {
        let index = replacements.len();
        replacements.push(anchor(
            &normalize_url(&captures[2]),
            &escape_html(&captures[1]),
        ));
        format!("__EMAIL_LINK_{index}__")
    });
    let escaped = escape_html(&with_markers);
    let linked = PLAIN_URL.replace_all(&escaped, |captures: &Captures| {
        let url = normalize_url(&captures[0]);
        anchor(&url, &escape_html(&url))
    });
    let bold = BOLD.replace_all(&linked, "<strong>$1</strong>");
    replacements
        .iter()
        .enumerate()
        .fold(bold.into_owned(), |html, (index, replacement)| {
            html.replacen(&format!("__EMAIL_LINK_{index}__"), replacement, 1)
        })
}

/// Drops a CSS preamble pasted above the greeting of a plain-text body.
fn strip_css_noise(value: &str) -> String {
    let without_styles = STYLE_BLOCK.replace_all(value, "").into_owned();
    let Some(greeting) = GREETING.find(&without_styles) else {
        return without_styles;
    };
    if greeting.start() == 0 || !CSS_NOISE.is_match(&without_styles[..greeting.start()]) {
        return without_styles;
    }
    without_styles[greeting.start()..].trim_start().to_string()
}

/// Plain text as HTML paragraphs and bullet lists.
pub fn plain_email_to_html(value: &str) -> String {
    let text = strip_css_noise(value)
        .replace("\r\n", "\n")
        .replace('\r', "\n");
    let mut blocks: Vec<String> = Vec::new();
    let mut paragraph: Vec<&str> = Vec::new();
    let mut list: Vec<String> = Vec::new();
    let mut pending_item: Option<Vec<&str>> = None;

    let flush_paragraph = |paragraph: &mut Vec<&str>, blocks: &mut Vec<String>| {
        if !paragraph.is_empty() {
            let lines: Vec<String> = paragraph.iter().map(|line| format_inline(line)).collect();
            blocks.push(format!("<p>{}</p>", lines.join("<br>")));
            paragraph.clear();
        }
    };
    let flush_list = |list: &mut Vec<String>, blocks: &mut Vec<String>| {
        if !list.is_empty() {
            let items = list.iter().fold(String::new(), |mut items, item| {
                items.push_str("<li>");
                items.push_str(&format_inline(item));
                items.push_str("</li>");
                items
            });
            blocks.push(format!("<ul>{items}</ul>"));
            list.clear();
        }
    };
    let flush_pending = |pending: &mut Option<Vec<&str>>, list: &mut Vec<String>| {
        if let Some(parts) = pending.take() {
            let item = WHITESPACE
                .replace_all(&parts.join(" "), " ")
                .trim()
                .to_string();
            if !item.is_empty() {
                list.push(item);
            }
        }
    };

    for line in text.split('\n') {
        let trimmed = line.trim();
        if trimmed == "-" {
            flush_paragraph(&mut paragraph, &mut blocks);
            flush_pending(&mut pending_item, &mut list);
            pending_item = Some(Vec::new());
            continue;
        }
        if trimmed.is_empty() {
            let had_content = !paragraph.is_empty() || pending_item.is_some() || !list.is_empty();
            flush_paragraph(&mut paragraph, &mut blocks);
            flush_pending(&mut pending_item, &mut list);
            flush_list(&mut list, &mut blocks);
            if had_content || !blocks.is_empty() {
                blocks.push("<br>".to_string());
            }
            continue;
        }
        if let Some(bullet) = BULLET.captures(trimmed) {
            flush_paragraph(&mut paragraph, &mut blocks);
            flush_pending(&mut pending_item, &mut list);
            list.push(bullet[1].to_string());
            continue;
        }
        if let Some(parts) = pending_item.as_mut() {
            parts.push(trimmed);
            continue;
        }
        flush_list(&mut list, &mut blocks);
        paragraph.push(line);
    }
    flush_paragraph(&mut paragraph, &mut blocks);
    flush_pending(&mut pending_item, &mut list);
    flush_list(&mut list, &mut blocks);
    blocks.concat()
}

/// Paragraphs without margins and images capped at signature size.
fn tight_paragraphs(html: &str) -> String {
    let paragraphs = P_OPEN.replace_all(html, |captures: &Captures| {
        match captures.get(1).map(|m| m.as_str()) {
            Some(attrs) if STYLE_ATTR.is_match(attrs) => {
                if MARGIN.is_match(attrs) {
                    format!("<p{attrs}>")
                } else {
                    format!("<p{}>", STYLE_ATTR.replace(attrs, "style=\"margin: 0; "))
                }
            }
            Some(attrs) => format!("<p{attrs} style=\"margin: 0;\">"),
            None => "<p style=\"margin: 0;\">".to_string(),
        }
    });
    IMG_OPEN
        .replace_all(&paragraphs, |captures: &Captures| {
            let attrs = captures.get(1).map_or("", |m| m.as_str());
            if STYLE_ATTR.is_match(attrs) {
                if MAX_WIDTH.is_match(attrs) {
                    format!("<img{attrs}>")
                } else {
                    format!(
                        "<img{}>",
                        STYLE_ATTR.replace(
                            attrs,
                            "style=\"max-width: 220px; height: auto; display: block; margin-top: 8px; "
                        )
                    )
                }
            } else {
                format!(
                    "<img{attrs} style=\"max-width: 220px; height: auto; display: block; margin-top: 8px;\">"
                )
            }
        })
        .into_owned()
}

/// The signature block of the HTML body ("--" line, then the signature).
fn signature_html(signature: Option<&str>) -> String {
    let raw = signature.unwrap_or("").trim();
    if raw.is_empty() {
        return String::new();
    }
    let html = if is_html_markup(raw) {
        if is_empty_rich_text(raw) {
            return String::new();
        }
        SANITIZER.clean(raw).to_string().trim().to_string()
    } else {
        format!(
            "<p style=\"margin: 0; white-space: pre-wrap;\">{}</p>",
            escape_html(raw).replace('\n', "<br>")
        )
    };
    if html.is_empty() {
        return String::new();
    }
    let inner = linkify_outside_anchors(&tight_paragraphs(&html), &SIGNATURE_URL, |prefix, url| {
        format!(
            "{prefix}<a href=\"{}\" target=\"_blank\" rel=\"noreferrer\">{}</a>",
            escape_html(url),
            escape_html(url)
        )
    });
    let dash = if LEADING_DASH.is_match(&inner) {
        ""
    } else {
        "<div style=\"margin-bottom: 4px;\">--</div>"
    };
    format!("<div style=\"margin-top: 16px; {SIGNATURE_STYLE}\">{dash}{inner}</div>")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_body_gets_paragraphs_signature_and_opt_out() {
        let bodies = outgoing_bodies("Hi Nina,\n\nYour quote is ready.", Some("Dina\nGranite Co"));
        assert_eq!(
            bodies.text,
            format!(
                "Hi Nina,\n\nYour quote is ready.\n\n—\nDina\nGranite Co\n\n{SOFT_OPT_OUT_LINE}"
            )
        );
        assert!(
            bodies
                .html
                .contains("<p>Hi Nina,</p><br><p>Your quote is ready.</p>")
        );
        assert!(
            bodies
                .html
                .contains("<div style=\"margin-bottom: 4px;\">--</div>")
        );
        assert!(bodies.html.contains("Dina<br>Granite Co"));
        assert!(bodies.html.contains(SOFT_OPT_OUT_LINE));
    }

    #[test]
    fn html_body_is_sanitized_and_converted_to_text() {
        let bodies = outgoing_bodies(
            "<p>Hello <strong>there</strong></p><script>alert(1)</script><p><a href=\"https://example.com/q\">quote</a></p>",
            None,
        );
        assert!(!bodies.html.contains("<script"));
        assert!(bodies.html.contains("<strong>there</strong>"));
        assert!(bodies.html.contains("target=\"_blank\""));
        assert!(
            bodies
                .text
                .starts_with("Hello there\nquote (https://example.com/q)")
        );
    }

    #[test]
    fn bare_urls_become_links_but_anchors_stay() {
        let html =
            sanitize_body("<p>See https://example.com/a and <a href=\"https://x.io\">x</a></p>");
        assert!(html.contains(
            "<a href=\"https://example.com/a\" target=\"_blank\" rel=\"noreferrer\">https://example.com/a</a>"
        ));
        assert_eq!(html.matches("https://x.io").count(), 1);
    }

    #[test]
    fn forwarded_block_is_kept_as_its_own_paragraphs() {
        let body = "<p>FYI</p>\n---------- Forwarded message ---------\nFrom: a@b.com\nSubject: Hi";
        let bodies = outgoing_bodies(body, None);
        assert!(
            bodies
                .text
                .starts_with("FYI\n\n---------- Forwarded message ---------\nFrom: a@b.com")
        );
        assert!(bodies.html.contains("<p>FYI</p><br><p>---------- Forwarded message ---------<br>From: a@b.com<br>Subject: Hi</p>"));
    }

    #[test]
    fn signature_already_in_body_is_not_repeated() {
        let bodies = outgoing_bodies("Thanks\n\n—\nDina", Some("Dina"));
        assert_eq!(bodies.text.matches("Dina").count(), 1);
    }

    #[test]
    fn dashed_signature_gets_no_second_dash() {
        let bodies = outgoing_bodies("Hi", Some("-- \nDina"));
        assert!(bodies.text.contains("Hi\n\n-- \nDina"));
        assert!(!bodies.html.contains("margin-bottom: 4px;\">--</div>"));
    }

    #[test]
    fn html_signature_gets_tight_paragraphs_and_capped_images() {
        let html = signature_html(Some("<p>Dina</p><img src=\"https://x.io/logo.png\">"));
        assert!(html.contains("<p style=\"margin: 0;\">Dina</p>"));
        assert!(html.contains("max-width: 220px"));
    }

    #[test]
    fn plain_text_lists_and_inline_formatting() {
        assert_eq!(
            plain_email_to_html("Items:\n- one\n- two\n\n*Bold* [site](https://x.io)"),
            "<p>Items:</p><ul><li>one</li><li>two</li></ul><br><p><strong>Bold</strong> <a href=\"https://x.io\" target=\"_blank\" rel=\"noreferrer\">site</a></p>"
        );
        assert_eq!(plain_email_to_html("a < b & c"), "<p>a &lt; b &amp; c</p>");
    }

    #[test]
    fn entities_decode() {
        assert_eq!(
            html_to_plain_text("<p>a&nbsp;&amp;&#233;&#x41;</p>"),
            "a &éA"
        );
    }

    #[test]
    fn empty_rich_text() {
        assert!(is_empty_rich_text("<p><br></p>"));
        assert!(is_empty_rich_text("  "));
        assert!(!is_empty_rich_text("<p>x</p>"));
    }
}
