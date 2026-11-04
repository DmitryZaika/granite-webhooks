//! The company's web pages the 10DLC AI check reads ([`super::tendlc_check`]):
//! the quote form customers fill out, the privacy policy, the texting terms
//! and the website, fetched the way a carrier's reviewer opens them; and the
//! privacy and terms pages the quote form itself links to, when they are
//! other pages ([`policy_links`]).
//!
//! Each page is reduced to what a reviewer sees: its text and its form
//! (fields, checkboxes and whether they are checked or required, labels,
//! links), without scripts, styles or attributes that say nothing about
//! consent. A quote form in an iframe (JotForm, Google Forms, ...) is read
//! too. Only public addresses are fetched: a link to a private or loopback
//! address is refused (the stand-in sites of the tests aside).

use reqwest::redirect::Policy;
use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, SocketAddr};
use std::sync::LazyLock;
use std::time::Duration;
use url::Url;

/// One page's fetch, redirects included.
const FETCH_TIMEOUT: Duration = Duration::from_secs(25);
/// The most of a page read; the rest is dropped.
const MAX_BYTES: usize = 3 * 1024 * 1024;
/// Redirects followed.
const MAX_REDIRECTS: usize = 6;
/// Iframes of the quote form read (an embedded form builder).
const MAX_FRAMES: usize = 3;
/// A browser's user agent: some sites refuse other clients, and the
/// carriers' reviewers use browsers.
const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
                          (KHTML, like Gecko) Chrome/130.0.0.0 Safari/537.36";

/// Which of the company's pages.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PageKind {
    QuoteForm,
    PrivacyPolicy,
    Terms,
    Website,
    /// A privacy page the quote form links to that is not the registered one.
    LinkedPrivacy,
    /// A terms (or texting policy) page the quote form links to that is not
    /// the registered one.
    LinkedTerms,
}

impl PageKind {
    /// The page's location in a report (`page:quoteForm`).
    pub const fn location(self) -> &'static str {
        match self {
            Self::QuoteForm => "page:quoteForm",
            Self::PrivacyPolicy => "page:privacyPolicy",
            Self::Terms => "page:terms",
            Self::Website => "page:website",
            Self::LinkedPrivacy => "page:linkedPrivacy",
            Self::LinkedTerms => "page:linkedTerms",
        }
    }

    /// The page, for people.
    pub const fn label(self) -> &'static str {
        match self {
            Self::QuoteForm => "Your quote form",
            Self::PrivacyPolicy => "Your privacy policy",
            Self::Terms => "Your texting terms",
            Self::Website => "Your website",
            Self::LinkedPrivacy => "The privacy page your quote form links to",
            Self::LinkedTerms => "The terms page your quote form links to",
        }
    }

    pub fn from_location(location: &str) -> Option<Self> {
        [
            Self::QuoteForm,
            Self::PrivacyPolicy,
            Self::Terms,
            Self::Website,
            Self::LinkedPrivacy,
            Self::LinkedTerms,
        ]
        .into_iter()
        .find(|kind| kind.location() == location)
    }
}

/// How the fetch went.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Fetched {
    /// The page opened: its content is read.
    Read,
    /// The page does not open for anyone (not found, a server error, no
    /// such site): the carriers will fail it too. Why, for people.
    Broken(String),
    /// We could not read it, though it may open in a browser (bot
    /// protection, a timeout, a refused address). Why, for people.
    Unreadable(String),
}

/// A page as the check reads it.
#[derive(Debug, Clone)]
pub struct Page {
    pub kind: PageKind,
    /// The link on the form.
    pub url: String,
    /// Where the redirects ended; `None` when it did not load.
    pub final_url: Option<String>,
    /// The HTTP status; `None` when there was no answer.
    pub status: Option<u16>,
    pub fetched: Fetched,
    /// The page reduced to its text, form and links (see [`clean`]).
    pub html: String,
    /// The page's text alone, to find the passages the AI quotes.
    pub text: String,
    /// Form fields on the page and its frames (inputs, selects, text areas).
    pub fields: usize,
    /// The quote form's iframes that were read.
    pub frames: Vec<Frame>,
}

/// An iframe of the quote form.
#[derive(Debug, Clone)]
pub struct Frame {
    pub url: String,
    pub fetched: Fetched,
    pub html: String,
    pub text: String,
    pub fields: usize,
}

// ---------------------------------------------------------------- reading

/// An address a page may be fetched from: not loopback, private,
/// link-local, unspecified or multicast.
pub fn public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => {
            !(ip.is_loopback()
                || ip.is_private()
                || ip.is_link_local()
                || ip.is_unspecified()
                || ip.is_broadcast()
                || ip.is_multicast()
                || ip.is_documentation()
                // 100.64.0.0/10, shared address space
                || (ip.octets()[0] == 100 && (ip.octets()[1] & 0xC0) == 64))
        }
        IpAddr::V6(ip) => {
            if let Some(v4) = ip.to_ipv4_mapped() {
                return public_ip(IpAddr::V4(v4));
            }
            let first = ip.segments()[0];
            !(ip.is_loopback()
                || ip.is_unspecified()
                || ip.is_multicast()
                // fc00::/7 unique local, fe80::/10 link-local
                || (first & 0xFE00) == 0xFC00
                || (first & 0xFFC0) == 0xFE80)
        }
    }
}

/// The host of a URL names a private address outright (`localhost`, an IP).
fn private_host(url: &Url) -> bool {
    match url.host() {
        Some(url::Host::Ipv4(ip)) => !public_ip(IpAddr::V4(ip)),
        Some(url::Host::Ipv6(ip)) => !public_ip(IpAddr::V6(ip)),
        Some(url::Host::Domain(host)) => {
            let host = host.trim_end_matches('.').to_ascii_lowercase();
            host == "localhost" || host.ends_with(".localhost") || host.ends_with(".internal")
        }
        None => true,
    }
}

/// Fetches the company's pages, the quote form's iframes too. Never fails:
/// each page says how its fetch went.
pub async fn read_pages(links: &[(PageKind, String)], allow_private: bool) -> Vec<Page> {
    let mut pages = Vec::new();
    for (kind, url) in links {
        let mut page = read_page(*kind, url, allow_private).await;
        if *kind == PageKind::QuoteForm && page.fetched == Fetched::Read {
            for src in frame_sources(&page.html).into_iter().take(MAX_FRAMES) {
                let (fetched, final_url, _, body) = fetch(&src, allow_private).await;
                let (html, text, fields) = match &fetched {
                    Fetched::Read => cleaned(&body, final_url.as_deref().unwrap_or(&src)),
                    _ => (String::new(), String::new(), 0),
                };
                page.fields += fields;
                page.frames.push(Frame {
                    url: src,
                    fetched,
                    html,
                    text,
                    fields,
                });
            }
        }
        pages.push(page);
    }
    pages
}

async fn read_page(kind: PageKind, url: &str, allow_private: bool) -> Page {
    let (fetched, final_url, status, body) = fetch(url, allow_private).await;
    let (html, text, fields) = match &fetched {
        Fetched::Read => cleaned(&body, final_url.as_deref().unwrap_or(url)),
        _ => (String::new(), String::new(), 0),
    };
    // A page that answered but shows nothing is drawn by scripts we do not
    // run: it may well work in a browser.
    let fetched = if fetched == Fetched::Read && text.split_whitespace().count() < 5 {
        Fetched::Unreadable(
            "The page has no text without JavaScript, so its content could not be read.".into(),
        )
    } else {
        fetched
    };
    Page {
        kind,
        url: url.to_string(),
        final_url,
        status,
        fetched,
        html,
        text,
        fields,
        frames: Vec::new(),
    }
}

/// One GET: how it went, the final URL, the status and the body.
async fn fetch(url: &str, allow_private: bool) -> (Fetched, Option<String>, Option<u16>, String) {
    let unreadable = |why: &str| {
        (
            Fetched::Unreadable(why.to_string()),
            None,
            None,
            String::new(),
        )
    };
    let Ok(parsed) = Url::parse(url) else {
        return (
            Fetched::Broken("The link is not a valid web address.".into()),
            None,
            None,
            String::new(),
        );
    };
    if !matches!(parsed.scheme(), "http" | "https") {
        return (
            Fetched::Broken("The link must start with https://.".into()),
            None,
            None,
            String::new(),
        );
    }
    let Some(host) = parsed.host_str().map(str::to_string) else {
        return (
            Fetched::Broken("The link has no website in it.".into()),
            None,
            None,
            String::new(),
        );
    };
    if !allow_private && private_host(&parsed) {
        return unreadable("The link points at a private address.");
    }
    // The address is checked before connecting, and the connection pinned
    // to it, so a name cannot lead to a private address.
    let port = parsed.port_or_known_default().unwrap_or(443);
    let addresses: Vec<SocketAddr> = match tokio::time::timeout(
        Duration::from_secs(10),
        tokio::net::lookup_host((host.as_str(), port)),
    )
    .await
    {
        Ok(Ok(found)) => found.collect(),
        Ok(Err(_)) => {
            return (
                Fetched::Broken(format!(
                    "The website {host} does not exist (no DNS record)."
                )),
                None,
                None,
                String::new(),
            );
        }
        Err(_) => return unreadable("Looking up the website took too long."),
    };
    let Some(address) = addresses.first().copied() else {
        return (
            Fetched::Broken(format!(
                "The website {host} does not exist (no DNS record)."
            )),
            None,
            None,
            String::new(),
        );
    };
    if !allow_private && addresses.iter().any(|address| !public_ip(address.ip())) {
        return unreadable("The link points at a private address.");
    }
    let client = match common::http::client_builder()
        .resolve(&host, address)
        .redirect(Policy::custom(move |attempt| {
            if attempt.previous().len() >= MAX_REDIRECTS {
                attempt.error("too many redirects")
            } else if !allow_private && private_host(attempt.url()) {
                attempt.stop()
            } else {
                attempt.follow()
            }
        }))
        .user_agent(USER_AGENT)
        .timeout(FETCH_TIMEOUT)
        .build()
    {
        Ok(client) => client,
        Err(_) => return unreadable("The page could not be opened."),
    };
    let response = match client
        .get(parsed.as_str())
        .header("Accept", "text/html,application/xhtml+xml;q=0.9,*/*;q=0.8")
        .header("Accept-Language", "en-US,en;q=0.9")
        .send()
        .await
    {
        Ok(response) => response,
        Err(err) if err.is_timeout() => return unreadable("The page took too long to open."),
        Err(err) if err.is_connect() => {
            return (
                Fetched::Broken(format!("The website {host} does not answer.")),
                None,
                None,
                String::new(),
            );
        }
        Err(_) => return unreadable("The page could not be opened."),
    };
    let status = response.status().as_u16();
    let final_url = response.url().to_string();
    let html_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_none_or(|value| {
            let value = value.to_ascii_lowercase();
            value.contains("html") || value.contains("text/plain")
        });
    let body = read_body(response).await;
    let fetched = if (200..300).contains(&status) {
        if html_type {
            Fetched::Read
        } else {
            Fetched::Broken("The link opens a file, not a web page.".into())
        }
    } else if (300..400).contains(&status) {
        Fetched::Unreadable("The page redirects somewhere it could not be followed.".into())
    } else if matches!(status, 401 | 403 | 429) || bot_challenge(&body) {
        Fetched::Unreadable(format!(
            "The website refused to show the page to our checker (HTTP {status})."
        ))
    } else if matches!(status, 404 | 410) {
        Fetched::Broken(format!(
            "The page does not exist (HTTP {status}: not found)."
        ))
    } else {
        Fetched::Broken(format!("The page does not open (HTTP {status})."))
    };
    (fetched, Some(final_url), Some(status), body)
}

/// The body, up to [`MAX_BYTES`].
async fn read_body(mut response: reqwest::Response) -> String {
    let mut bytes: Vec<u8> = Vec::new();
    while let Ok(Some(chunk)) = response.chunk().await {
        bytes.extend_from_slice(&chunk);
        if bytes.len() >= MAX_BYTES {
            bytes.truncate(MAX_BYTES);
            break;
        }
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

/// A bot protection page (Cloudflare's "Just a moment...", ...).
fn bot_challenge(body: &str) -> bool {
    let head: String = body.chars().take(20_000).collect::<String>().to_lowercase();
    head.contains("cf-chl")
        || head.contains("challenge-platform")
        || head.contains("<title>just a moment")
        || head.contains("attention required! | cloudflare")
}

// --------------------------------------------------------------- cleaning

/// Tags kept: the page's structure, text and forms. Every other tag goes,
/// its text kept; scripts, styles and drawings go with their content.
const TAGS: &[&str] = &[
    "a", "article", "b", "br", "button", "div", "em", "fieldset", "footer", "form", "h1", "h2",
    "h3", "h4", "h5", "h6", "header", "i", "iframe", "input", "label", "legend", "li", "main",
    "nav", "ol", "option", "p", "section", "select", "small", "span", "strong", "table", "td",
    "textarea", "th", "title", "tr", "u", "ul",
];
const DROPPED_WITH_CONTENT: &[&str] = &["script", "style", "svg", "noscript", "template", "head"];
/// Attributes that say what a field is and whether it is checked or
/// required; links.
const TAG_ATTRIBUTES: &[(&str, &[&str])] = &[
    ("a", &["href"]),
    ("button", &["type", "name"]),
    ("form", &["action", "method", "name", "id"]),
    ("iframe", &["src", "title"]),
    (
        "input",
        &[
            "type",
            "name",
            "id",
            "value",
            "placeholder",
            "required",
            "checked",
            "disabled",
        ],
    ),
    ("label", &["for"]),
    ("option", &["value", "selected"]),
    ("select", &["name", "id", "required"]),
    ("textarea", &["name", "id", "placeholder", "required"]),
];
const GENERIC_ATTRIBUTES: &[&str] = &[
    "role",
    "aria-label",
    "aria-checked",
    "aria-required",
    "data-required",
];
/// Wrappers left empty once their attributes are gone.
const EMPTY_WRAPPER: &str = r"<(div|span|section|header|footer|main|nav|article|p|li|ul|ol|b|i|u|em|strong|small|label|td|th|tr|table)>\s*</(div|span|section|header|footer|main|nav|article|p|li|ul|ol|b|i|u|em|strong|small|label|td|th|tr|table)>";

static EMPTY: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(EMPTY_WRAPPER).expect("valid pattern"));
static SPACES: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"\s+").expect("valid pattern"));
static TAG: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"<[^>]*>").expect("valid pattern"));
static FIELD: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"<(input|select|textarea)\b").expect("valid pattern"));
static HIDDEN_INPUT: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r#"<input\b[^>]*type="hidden"[^>]*>"#).expect("valid pattern")
});
static IFRAME_SRC: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r#"<iframe\b[^>]*\bsrc="([^"]+)""#).expect("valid pattern"));
/// A link of cleaned HTML (`href` is its only attribute) and its content.
static ANCHOR: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r#"(?s)<a href="([^"]+)">(.*?)</a>"#).expect("valid pattern")
});

/// Iframes that are never a form.
const NOT_FORMS: &[&str] = &[
    "youtube.com",
    "youtube-nocookie.com",
    "vimeo.com",
    "google.com/maps",
    "maps.google",
    "googletagmanager.com",
    "facebook.com",
    "recaptcha",
    "hcaptcha",
    "instagram.com",
    "doubleclick.net",
];

/// `html` reduced to what a reviewer reads, with relative links made
/// absolute against `base`: the cleaned HTML, its text, and how many form
/// fields it has (hidden inputs aside).
pub fn cleaned(html: &str, base: &str) -> (String, String, usize) {
    let mut builder = ammonia::Builder::empty();
    let tag_attributes: HashMap<&str, HashSet<&str>> = TAG_ATTRIBUTES
        .iter()
        .map(|(tag, attributes)| (*tag, attributes.iter().copied().collect()))
        .collect();
    builder
        .tags(TAGS.iter().copied().collect())
        .clean_content_tags(DROPPED_WITH_CONTENT.iter().copied().collect())
        .tag_attributes(tag_attributes)
        .generic_attributes(GENERIC_ATTRIBUTES.iter().copied().collect())
        .link_rel(None)
        .strip_comments(true);
    if let Ok(base) = Url::parse(base) {
        builder.url_relative(ammonia::UrlRelative::RewriteWithBase(base));
    }
    let mut out = builder.clean(html).to_string();
    out = SPACES.replace_all(&out, " ").into_owned();
    // Wrappers nest: empty ones go until none is left.
    loop {
        let next = EMPTY.replace_all(&out, "").into_owned();
        if next.len() == out.len() {
            break;
        }
        out = next;
    }
    let text = text_of(&out);
    let all_inputs = FIELD.find_iter(&out).count();
    let hidden = HIDDEN_INPUT.find_iter(&out).count();
    (out, text, all_inputs.saturating_sub(hidden))
}

/// The text of cleaned HTML, entities decoded, whitespace collapsed.
pub fn text_of(html: &str) -> String {
    let text = TAG.replace_all(html, " ");
    let text = text
        .replace("&nbsp;", " ")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&amp;", "&");
    SPACES.replace_all(text.trim(), " ").into_owned()
}

/// The iframes of cleaned HTML that may hold a form, as absolute links.
fn frame_sources(html: &str) -> Vec<String> {
    let mut found = Vec::new();
    for capture in IFRAME_SRC.captures_iter(html) {
        let src = capture[1].replace("&amp;", "&");
        let lower = src.to_lowercase();
        if !(lower.starts_with("https://") || lower.starts_with("http://"))
            || NOT_FORMS.iter().any(|skip| lower.contains(skip))
            || found.contains(&src)
        {
            continue;
        }
        found.push(src);
    }
    found
}

/// The links of cleaned HTML to a privacy policy and to terms (a texting or
/// SMS policy counts as terms), by their address or their words; absolute
/// web links only, once each, in page order.
pub fn policy_links(html: &str) -> (Vec<String>, Vec<String>) {
    let mut privacy: Vec<String> = Vec::new();
    let mut terms: Vec<String> = Vec::new();
    for capture in ANCHOR.captures_iter(html) {
        let href = capture[1].replace("&amp;", "&");
        let lower = href.to_lowercase();
        if !(lower.starts_with("https://") || lower.starts_with("http://")) {
            continue;
        }
        let words = format!("{lower} {}", text_of(&capture[2]).to_lowercase());
        if words.contains("privacy") && !privacy.contains(&href) {
            privacy.push(href.clone());
        }
        if (words.contains("terms") || words.contains("conditions") || words.contains("sms"))
            && !terms.contains(&href)
        {
            terms.push(href);
        }
    }
    (privacy, terms)
}

/// Letters and digits only, lower case: text compared whatever its
/// punctuation, spacing, quotes or entities.
pub fn comparable(text: &str) -> String {
    text.chars()
        .filter(char::is_ascii_alphanumeric)
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const PAGE: &str = r#"<!doctype html><html><head><title>Get a quote</title>
        <script>var secret = "x";</script><style>.a{color:red}</style></head>
        <body><div class="hero"><div><div></div></div>
        <h1 class="big">Free quote</h1>
        <form action="/send" method="post" class="f">
          <label for="phone">Phone</label>
          <input type="tel" id="phone" name="phone" required class="x" data-foo="1">
          <input type="hidden" name="token" value="abc">
          <input type="checkbox" id="sms" name="sms" checked onclick="evil()">
          <label for="sms">I agree to get texts from Acme Stone. Msg &amp; data rates may apply.</label>
          <a href="/privacy">Privacy Policy</a>
          <iframe src="https://form.jotform.com/123"></iframe>
          <iframe src="https://www.youtube.com/embed/x"></iframe>
          <button type="submit">Send</button>
        </form></div></body></html>"#;

    #[test]
    fn a_page_keeps_its_form_text_and_links() {
        let (html, text, fields) = cleaned(PAGE, "https://acmestone.com/quote");
        assert!(!html.contains("secret") && !html.contains("color:red"));
        assert!(
            !html.contains("class=") && !html.contains("onclick") && !html.contains("data-foo")
        );
        assert!(
            html.contains(r#"<input type="tel" id="phone" name="phone" required="">"#),
            "{html}"
        );
        assert!(
            html.contains(r#"<input type="checkbox" id="sms" name="sms" checked="">"#),
            "{html}"
        );
        assert!(
            html.contains(r#"<a href="https://acmestone.com/privacy">"#),
            "{html}"
        );
        assert!(!html.contains("<div></div>"));
        assert_eq!(fields, 2);
        assert!(text.contains("I agree to get texts from Acme Stone. Msg & data rates may apply."));
        assert_eq!(
            frame_sources(&html),
            ["https://form.jotform.com/123".to_string()]
        );
    }

    #[test]
    fn only_public_addresses_are_read() {
        for ip in [
            "127.0.0.1",
            "10.1.2.3",
            "192.168.0.5",
            "169.254.169.254",
            "100.64.0.1",
            "::1",
            "fd00::1",
            "::ffff:10.0.0.1",
        ] {
            assert!(!public_ip(ip.parse().unwrap()), "{ip}");
        }
        for ip in ["8.8.8.8", "104.16.0.1", "2606:4700::1111"] {
            assert!(public_ip(ip.parse().unwrap()), "{ip}");
        }
        assert!(private_host(
            &Url::parse("http://localhost:9000/x").unwrap()
        ));
        assert!(private_host(&Url::parse("http://10.0.0.1/x").unwrap()));
        assert!(!private_host(
            &Url::parse("https://acmestone.com/x").unwrap()
        ));
    }

    #[test]
    fn the_policy_links_are_found_by_address_or_words() {
        let page = r#"<html><body><form><input type="checkbox" name="sms">
            <label>I agree to texts. <a href="/legal/sms-policy/">SMS Policy</a></label></form>
            <footer><a href="/your-privacy-choices/">Your Privacy Choices</a>
            <a href="/privacy-policy-terms-of-use/">Legal</a>
            <a href="/terms-and-conditions/"><span>Terms</span> &amp; Conditions</a>
            <a href="/air-conditioning/">Air conditioning</a>
            <a href="mailto:privacy@acmestone.com">Privacy questions</a>
            <a href="/your-privacy-choices/">Privacy</a></footer></body></html>"#;
        let (html, _, _) = cleaned(page, "https://acmestone.com/quote");
        let (privacy, terms) = policy_links(&html);
        assert_eq!(
            privacy,
            [
                "https://acmestone.com/your-privacy-choices/",
                "https://acmestone.com/privacy-policy-terms-of-use/",
            ]
        );
        assert_eq!(
            terms,
            [
                "https://acmestone.com/legal/sms-policy/",
                "https://acmestone.com/privacy-policy-terms-of-use/",
                "https://acmestone.com/terms-and-conditions/",
            ]
        );
    }

    #[test]
    fn quotes_compare_whatever_their_punctuation() {
        assert_eq!(
            comparable("Msg & data   rates “may” apply!"),
            comparable("msg data rates \"may\" apply")
        );
    }
}
