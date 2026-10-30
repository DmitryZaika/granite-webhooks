//! What code checks on the company's pages before the AI reads them
//! ([`super::tendlc_check`]): what a pattern settles, so the check never
//! leaves it to the AI. Each [`Fact`] names the check it fails and, when code
//! can write it, its fix (Telnyx's rejection codes in brackets):
//!
//! - the quote form links to a privacy policy and to terms (806), and they
//!   are the pages on the registration;
//! - its texts checkbox is there, not checked in advance and not required
//!   (803);
//! - the website shows the business phone, email and address the
//!   registration gives (804);
//! - the website says nothing of lead generation or SEO (708).
//!
//! [`linked_pages`] are the privacy and terms pages the quote form links to
//! that are not the registered ones: the check reads them too.

use super::tendlc::{TenDlcCampaignDraft, TenDlcDraft, UseCase, link_core};
use super::tendlc_check::{TenDlcAiFix, location_label};
use super::tendlc_pages::{Fetched, Page, PageKind, policy_links, text_of};
use regex::Regex;
use std::sync::LazyLock;

/// The quote form's privacy pages, and its terms pages, read besides the
/// registered ones.
const MAX_LINKED: usize = 2;
/// The words around a checkbox read for what it is about.
const LABEL_CHARS: usize = 300;

static CHECKBOX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"<input\b[^>]*\btype="checkbox"[^>]*>"#).expect("valid pattern"));
static PHONE_FIELD: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)<input\b[^>]*(type="tel"|(name|id|placeholder)="[^"]*phone[^"]*")"#)
        .expect("valid pattern")
});
/// Words of a checkbox about texts.
static TEXTS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(sms|texts?|texting|text messag\w*|text me)\b").expect("valid pattern")
});
/// A US or Canada phone number: its area code, exchange and line.
static PHONE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?:^|[^\d])(?:\+?1[\s.\-]?)?\(?([2-9]\d{2})\)?[\s.\-]?(\d{3})[\s.\-]?(\d{4})(?:$|[^\d])",
    )
    .expect("valid pattern")
});
static EMAIL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"[A-Za-z0-9._%+\-]+@[A-Za-z0-9.\-]+\.[A-Za-z]{2,}").expect("valid pattern")
});
/// What carriers decline as lead generation (708).
static LEAD_GEN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(lead\s+generation|lead\s+gen|(buy|sell)(ing)?\s+leads|seo)\b")
        .expect("valid pattern")
});

/// One thing code found on the pages.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fact {
    /// The check it fails (`quote_form_links`, ...).
    pub check: &'static str,
    /// What was found, for the AI and the superusers.
    pub fact: String,
    /// The fix, when code can write it; the AI writes the others.
    pub fix: Option<TenDlcAiFix>,
}

/// The first page of `kind` that was read.
fn read(pages: &[Page], kind: PageKind) -> Option<&Page> {
    pages
        .iter()
        .find(|page| page.kind == kind && page.fetched == Fetched::Read)
}

/// The cleaned HTML of a page and its frames.
fn htmls(page: &Page) -> Vec<&str> {
    std::iter::once(page.html.as_str())
        .chain(page.frames.iter().map(|frame| frame.html.as_str()))
        .collect()
}

/// The quote form's privacy and terms links (its frames' too), once each.
fn form_links(form: &Page) -> (Vec<String>, Vec<String>) {
    let mut privacy: Vec<String> = Vec::new();
    let mut terms: Vec<String> = Vec::new();
    for html in htmls(form) {
        let (more_privacy, more_terms) = policy_links(html);
        for (list, more) in [(&mut privacy, more_privacy), (&mut terms, more_terms)] {
            for link in more {
                if !list.contains(&link) {
                    list.push(link);
                }
            }
        }
    }
    (privacy, terms)
}

/// Where pages of `kinds` are: their links and where they ended, as
/// [`link_core`]s.
fn cores_of(pages: &[Page], kinds: &[PageKind]) -> Vec<String> {
    pages
        .iter()
        .filter(|page| kinds.contains(&page.kind))
        .flat_map(|page| {
            std::iter::once(link_core(&page.url)).chain(page.final_url.as_deref().map(link_core))
        })
        .collect()
}

/// The privacy and terms pages the quote form links to that are not the
/// registered ones, to read too ([`MAX_LINKED`] of each; a page linked as
/// both is read once).
pub fn linked_pages(pages: &[Page]) -> Vec<(PageKind, String)> {
    let Some(form) = read(pages, PageKind::QuoteForm) else {
        return Vec::new();
    };
    let (privacy, terms) = form_links(form);
    let mut seen = cores_of(pages, &[PageKind::PrivacyPolicy, PageKind::Terms]);
    let mut found = Vec::new();
    for (kind, links) in [
        (PageKind::LinkedPrivacy, privacy),
        (PageKind::LinkedTerms, terms),
    ] {
        let mut taken = 0;
        for link in links {
            let core = link_core(&link);
            if taken == MAX_LINKED || seen.contains(&core) {
                continue;
            }
            seen.push(core);
            found.push((kind, link));
            taken += 1;
        }
    }
    found
}

/// A fix code wrote.
fn fix(
    pages: &[Page],
    location: &str,
    problem: String,
    current: String,
    replacement: String,
) -> TenDlcAiFix {
    let link = PageKind::from_location(location).and_then(|kind| {
        pages
            .iter()
            .find(|page| page.kind == kind)
            .map(|page| page.url.clone())
    });
    TenDlcAiFix {
        location: location.to_string(),
        label: location_label(location),
        link,
        problem,
        current,
        replacement,
    }
}

/// The kinds of texts, in words ("customer care, account notifications and
/// marketing offers").
fn kinds_of_texts(campaign: &TenDlcCampaignDraft) -> String {
    let kinds: Vec<UseCase> = if campaign.sub_usecases.is_empty() {
        campaign.usecase.into_iter().collect()
    } else {
        campaign.sub_usecases.clone()
    };
    let words: Vec<&str> = kinds
        .iter()
        .map(|kind| match kind {
            UseCase::CustomerCare => "customer care",
            UseCase::AccountNotification => "account notifications",
            UseCase::DeliveryNotification => "delivery updates",
            UseCase::Marketing => "marketing offers",
            UseCase::PollingVoting => "surveys",
            UseCase::TwoFactor => "sign-in codes",
            UseCase::SecurityAlert => "security alerts",
            UseCase::FraudAlert => "fraud alerts",
            UseCase::PublicServiceAnnouncement => "public service announcements",
            UseCase::HigherEducation => "school updates",
            UseCase::Mixed | UseCase::LowVolume => "updates",
        })
        .collect();
    match words.as_slice() {
        [] => "my quote and project".to_string(),
        [one] => (*one).to_string(),
        [first @ .., last] => format!("{} and {last}", first.join(", ")),
    }
}

/// The words of a texts checkbox as Telnyx's opt-in form guide has them.
pub fn consent_words(draft: &TenDlcDraft) -> String {
    let campaign = &draft.campaign;
    let marketing = campaign.usecase == Some(UseCase::Marketing)
        || campaign.sub_usecases.contains(&UseCase::Marketing);
    format!(
        "By checking this box, I agree to receive text messages from {name} about {kinds}. \
         Message frequency varies. Message and data rates may apply. Reply STOP to opt out. \
         Reply HELP for help.{condition} We will not share mobile information with third \
         parties for promotional or marketing purposes.",
        name = draft.brand.display_name,
        kinds = kinds_of_texts(campaign),
        condition = if marketing {
            " Consent is not a condition of purchase."
        } else {
            ""
        },
    )
}

/// Everything code finds on the pages.
pub fn facts(draft: &TenDlcDraft, pages: &[Page]) -> Vec<Fact> {
    let mut facts = Vec::new();
    if let Some(form) = read(pages, PageKind::QuoteForm) {
        link_facts(draft, pages, form, &mut facts);
        checkbox_facts(draft, pages, form, &mut facts);
    }
    if read(pages, PageKind::Website).is_some() {
        contact_facts(draft, pages, &mut facts);
        lead_facts(pages, &mut facts);
    }
    facts
}

/// The quote form links to a privacy policy and to terms, and those are
/// the pages on the registration (the carriers open both and compare).
fn link_facts(draft: &TenDlcDraft, pages: &[Page], form: &Page, facts: &mut Vec<Fact>) {
    let campaign = &draft.campaign;
    let (privacy, terms) = form_links(form);
    let mut missing: Vec<String> = Vec::new();
    let mut names: Vec<&str> = Vec::new();
    if privacy.is_empty() {
        missing.push(format!("Privacy Policy: {}", campaign.privacy_policy_link));
        names.push("privacy policy");
    }
    if terms.is_empty() {
        missing.push(format!("Terms: {}", campaign.terms_and_conditions_link));
        names.push("terms");
    }
    if !missing.is_empty() {
        let which = names.join(" or ");
        facts.push(Fact {
            check: "quote_form_links",
            fact: format!("The quote form page has no link to the {which}."),
            fix: Some(fix(
                pages,
                PageKind::QuoteForm.location(),
                format!(
                    "Carriers require a link to your {} on the quote form, next to the texts \
                     checkbox. Add it.",
                    names.join(" and ")
                ),
                String::new(),
                missing.join("\n"),
            )),
        });
    }
    for (links, registered, linked, location, check, name, current) in [
        (
            &privacy,
            PageKind::PrivacyPolicy,
            PageKind::LinkedPrivacy,
            "campaign.privacyPolicyLink",
            "privacy_opens",
            "privacy policy",
            &campaign.privacy_policy_link,
        ),
        (
            &terms,
            PageKind::Terms,
            PageKind::LinkedTerms,
            "campaign.termsAndConditionsLink",
            "terms_opens",
            "terms",
            &campaign.terms_and_conditions_link,
        ),
    ] {
        let Some(first) = links.first() else {
            continue;
        };
        let ours = cores_of(pages, &[registered]);
        let mut theirs: Vec<String> = links.iter().map(|link| link_core(link)).collect();
        theirs.extend(cores_of(pages, &[linked]));
        if ours.is_empty() || ours.iter().any(|core| theirs.contains(core)) {
            continue;
        }
        facts.push(Fact {
            check,
            fact: format!(
                "The {name} on the registration ({current}) is not a page the quote form links to \
                 ({}).",
                links.join(", ")
            ),
            fix: Some(fix(
                pages,
                location,
                format!(
                    "Carriers open the {name} your quote form links to and compare it with the \
                     registration's: give the same page."
                ),
                current.clone(),
                first.clone(),
            )),
        });
    }
}

/// The words before and after a checkbox (up to the fields around it).
fn around(html: &str, start: usize, end: usize) -> (String, String) {
    let head = &html[..start];
    let from = head
        .rfind("<input")
        .and_then(|at| head[at..].find('>').map(|close| at + close + 1))
        .unwrap_or(0);
    let before: Vec<char> = text_of(&head[from..]).chars().collect();
    let before: String = before[before.len().saturating_sub(LABEL_CHARS)..]
        .iter()
        .collect();
    let rest = &html[end..];
    let to = rest.find("<input").unwrap_or(rest.len());
    let after: String = text_of(&rest[..to]).chars().take(LABEL_CHARS).collect();
    (before, after)
}

/// The texts checkbox: there when the form asks for a phone number, not
/// checked in advance and not required (Telnyx's code 803).
fn checkbox_facts(draft: &TenDlcDraft, pages: &[Page], form: &Page, facts: &mut Vec<Fact>) {
    if form.fields == 0 {
        // Drawn by scripts: the AI says it could not tell.
        return;
    }
    let mut any = false;
    let mut phone_field = false;
    for html in htmls(form) {
        phone_field |= PHONE_FIELD.is_match(html);
        for tag in CHECKBOX.find_iter(html) {
            any = true;
            let (before, after) = around(html, tag.start(), tag.end());
            let words = if TEXTS.is_match(&after) {
                after
            } else if TEXTS.is_match(&before) {
                before
            } else {
                continue;
            };
            let input = tag.as_str();
            let checked = input.contains(" checked") || input.contains("aria-checked=\"true\"");
            let required = input.contains(" required")
                || input.contains("aria-required=\"true\"")
                || input.contains(" data-required");
            let setting = match (checked, required) {
                (true, true) => "starts unchecked and is optional",
                (true, false) => "starts unchecked",
                (false, true) => "is optional (the form can be sent without it)",
                (false, false) => continue,
            };
            facts.push(Fact {
                check: "quote_form_checkbox",
                fact: format!(
                    "The texts checkbox is {}.",
                    match (checked, required) {
                        (true, true) => "checked in advance and required",
                        (true, false) => "checked in advance",
                        _ => "required to send the form",
                    }
                ),
                fix: Some(fix(
                    pages,
                    PageKind::QuoteForm.location(),
                    format!(
                        "Customers must choose texts themselves: in the form's settings, make sure \
                         the texts checkbox {setting}. Use these words for it."
                    ),
                    words,
                    consent_words(draft),
                )),
            });
            return;
        }
    }
    if !any && phone_field {
        facts.push(Fact {
            check: "quote_form_checkbox",
            fact: "The quote form asks for a phone number but has no checkbox to agree to texts."
                .into(),
            fix: Some(fix(
                pages,
                PageKind::QuoteForm.location(),
                "Carriers require a checkbox to agree to texts next to the phone field, not \
                 checked in advance and not required to send the form. Add one with these words."
                    .into(),
                String::new(),
                consent_words(draft),
            )),
        });
    }
}

/// `+13175481060` as `(317) 548-1060`.
fn display_phone(digits: &str) -> String {
    if digits.len() == 10 {
        format!("({}) {}-{}", &digits[..3], &digits[3..6], &digits[6..])
    } else {
        digits.to_string()
    }
}

/// The 10 digits of a US or Canada number; `None` when it is not one.
fn phone_digits(text: &str) -> Option<String> {
    let digits: String = text.chars().filter(char::is_ascii_digit).collect();
    match digits.len() {
        10 => Some(digits),
        11 if digits.starts_with('1') => Some(digits[1..].to_string()),
        _ => None,
    }
}

/// Values found, most often first (then in page order).
fn ranked(found: Vec<String>) -> Vec<String> {
    let mut counted: Vec<(String, usize)> = Vec::new();
    for value in found {
        match counted.iter_mut().find(|(seen, _)| *seen == value) {
            Some((_, count)) => *count += 1,
            None => counted.push((value, 1)),
        }
    }
    counted.sort_by(|a, b| b.1.cmp(&a.1));
    counted.into_iter().map(|(value, _)| value).collect()
}

/// The business phone, email and address on the registration are on the
/// website, where carriers check them (Telnyx's code 804). When the website
/// shows another phone or email, the fix is the website's; what it shows
/// none of is to be added to it.
fn contact_facts(draft: &TenDlcDraft, pages: &[Page], facts: &mut Vec<Fact>) {
    let brand = &draft.brand;
    let read: Vec<&Page> = pages
        .iter()
        .filter(|page| page.fetched == Fetched::Read)
        .collect();
    let mut phones: Vec<String> = Vec::new();
    let mut emails: Vec<String> = Vec::new();
    let mut text = String::new();
    for page in &read {
        for html in htmls(page) {
            for found in PHONE.captures_iter(html) {
                phones.push(format!("{}{}{}", &found[1], &found[2], &found[3]));
            }
            for found in EMAIL.find_iter(html) {
                let email = found.as_str().to_lowercase();
                let image = [".png", ".jpg", ".jpeg", ".gif", ".svg", ".webp"]
                    .iter()
                    .any(|extension| email.ends_with(extension));
                if !image {
                    emails.push(email);
                }
            }
        }
        text.push_str(&page.text);
        text.push(' ');
    }
    let phones = ranked(phones);
    let emails = ranked(emails);
    let mut add: Vec<String> = Vec::new();

    if let Some(ours) = phone_digits(&brand.phone)
        && !phones.contains(&ours)
    {
        match phones.first() {
            Some(theirs) => facts.push(Fact {
                check: "website",
                fact: format!(
                    "The business phone on the registration, {}, is not on the website; it shows \
                     {}.",
                    display_phone(&ours),
                    display_phone(theirs)
                ),
                fix: Some(fix(
                    pages,
                    "brand.phone",
                    format!(
                        "Carriers check the registration's business phone on your website: it \
                         shows {}, not {}. Use the website's number, or add this one to your home \
                         page.",
                        display_phone(theirs),
                        display_phone(&ours)
                    ),
                    brand.phone.clone(),
                    format!("+1{theirs}"),
                )),
            }),
            None => add.push(format!("Phone: {}", display_phone(&ours))),
        }
    }

    let ours = brand.email.trim().to_lowercase();
    if !ours.is_empty() && !emails.contains(&ours) {
        let host = link_core(&brand.website);
        let host = host.split('/').next().unwrap_or_default().to_string();
        let theirs = emails
            .iter()
            .find(|email| !host.is_empty() && email.ends_with(&format!("@{host}")))
            .or_else(|| emails.first());
        match theirs {
            Some(theirs) => facts.push(Fact {
                check: "website",
                fact: format!(
                    "The business email on the registration, {ours}, is not on the website; it \
                     shows {theirs}."
                ),
                fix: Some(fix(
                    pages,
                    "brand.email",
                    format!(
                        "Carriers check the registration's business email on your website: it \
                         shows {theirs}, not {ours}. Use the website's email, or add this one to \
                         your home page."
                    ),
                    brand.email.clone(),
                    theirs.clone(),
                )),
            }),
            None => add.push(format!("Email: {}", brand.email.trim())),
        }
    }

    let number: String = brand
        .street
        .trim()
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    let zip: String = brand.postal_code.chars().take(5).collect();
    let shown = |part: &str| part.is_empty() || text.contains(part);
    if !brand.street.trim().is_empty() && !(shown(&number) && shown(&zip)) {
        add.push(format!(
            "Address: {}, {}, {} {}",
            brand.street.trim(),
            brand.city.trim(),
            brand.state.trim(),
            brand.postal_code.trim()
        ));
    }

    if !add.is_empty() {
        facts.push(Fact {
            check: "website",
            fact: format!(
                "The website does not show the business's {}.",
                add.iter()
                    .map(|line| line.split(':').next().unwrap_or_default().to_lowercase())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            fix: Some(fix(
                pages,
                PageKind::Website.location(),
                "Carriers check the business's phone, email and address on your website. Show them \
                 on your home page, for example in its footer."
                    .into(),
                String::new(),
                add.join("\n"),
            )),
        });
    }
}

/// The website mentions lead generation or SEO, which carriers decline
/// (Telnyx's code 708): the AI writes the change.
fn lead_facts(pages: &[Page], facts: &mut Vec<Fact>) {
    for kind in [PageKind::Website, PageKind::QuoteForm] {
        let Some(page) = read(pages, kind) else {
            continue;
        };
        if let Some(found) = LEAD_GEN.find(&page.text) {
            let start = page.text[..found.start()]
                .char_indices()
                .rev()
                .nth(80)
                .map_or(0, |(at, _)| at);
            let passage: String = page.text[start..].chars().take(200).collect();
            facts.push(Fact {
                check: "website",
                fact: format!(
                    "{} mentions \"{}\" (\"{}\"): carriers decline websites that mention lead \
                     generation or SEO.",
                    kind.label(),
                    found.as_str(),
                    passage.trim()
                ),
                fix: None,
            });
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::phone::tendlc::{normalized, suggestion};
    use crate::phone::tendlc_pages::cleaned;

    fn draft() -> TenDlcDraft {
        let mut draft = suggestion(
            "Acme Stone",
            Some("acmestone.com"),
            Some("+13175550100"),
            "office@acmestone.com",
            Some("Mark Manager"),
        );
        draft.brand.street = "100 Main St".into();
        draft.brand.city = "Indianapolis".into();
        draft.brand.state = "IN".into();
        draft.brand.postal_code = "46204".into();
        draft.campaign.optin_form_link = "https://acmestone.com/quote".into();
        draft.campaign.privacy_policy_link = "https://acmestone.com/privacy".into();
        draft.campaign.terms_and_conditions_link = "https://acmestone.com/terms".into();
        normalized(&draft)
    }

    fn page(kind: PageKind, url: &str, body: &str) -> Page {
        let (html, text, fields) = cleaned(body, url);
        Page {
            kind,
            url: url.into(),
            final_url: Some(url.into()),
            status: Some(200),
            fetched: Fetched::Read,
            html,
            text,
            fields,
            frames: Vec::new(),
        }
    }

    const FOOTER: &str = "<footer>Acme Stone · 100 Main St, Indianapolis, IN 46204 · \
        <a href=\"tel:+13175550100\">(317) 555-0100</a> · \
        <a href=\"mailto:office@acmestone.com\">office@acmestone.com</a></footer>";

    fn pages(form: &str, home: &str) -> Vec<Page> {
        vec![
            page(PageKind::QuoteForm, "https://acmestone.com/quote", form),
            page(
                PageKind::PrivacyPolicy,
                "https://acmestone.com/privacy",
                "<h1>Privacy</h1>",
            ),
            page(
                PageKind::Terms,
                "https://acmestone.com/terms",
                "<h1>Terms</h1>",
            ),
            page(PageKind::Website, "https://acmestone.com", home),
        ]
    }

    fn good_form() -> String {
        format!(
            "<form><input type=\"tel\" name=\"phone\" required>\
             <input type=\"checkbox\" name=\"sms\"><label>I agree to receive texts from Acme \
             Stone.</label> <a href=\"/privacy\">Privacy Policy</a> <a href=\"/terms\">Terms</a>\
             </form>{FOOTER}"
        )
    }

    #[test]
    fn a_good_site_has_no_facts() {
        let pages = pages(&good_form(), &format!("<h1>Acme Stone</h1>{FOOTER}"));
        assert_eq!(facts(&draft(), &pages), Vec::<Fact>::new());
        assert!(linked_pages(&pages).is_empty());
    }

    #[test]
    fn the_quote_forms_links_and_checkbox_are_checked() {
        let form = "<form><input type=\"tel\" name=\"phone\" required>\
            <label>Text me updates <input type=\"checkbox\" name=\"sms\" checked required></label>\
            <a href=\"/privacy-policy\">Privacy</a></form>";
        let pages = pages(form, &format!("<h1>Acme Stone</h1>{FOOTER}"));
        assert_eq!(
            linked_pages(&pages),
            [(
                PageKind::LinkedPrivacy,
                "https://acmestone.com/privacy-policy".to_string()
            )]
        );
        let found = facts(&draft(), &pages);
        let checks: Vec<&str> = found.iter().map(|fact| fact.check).collect();
        assert_eq!(
            checks,
            ["quote_form_links", "privacy_opens", "quote_form_checkbox"]
        );
        let links = found[0].fix.as_ref().unwrap();
        assert_eq!(links.location, "page:quoteForm");
        assert_eq!(links.replacement, "Terms: https://acmestone.com/terms");
        let privacy = found[1].fix.as_ref().unwrap();
        assert_eq!(privacy.location, "campaign.privacyPolicyLink");
        assert_eq!(privacy.current, "https://acmestone.com/privacy");
        assert_eq!(privacy.replacement, "https://acmestone.com/privacy-policy");
        assert_eq!(
            found[2].fact,
            "The texts checkbox is checked in advance and required."
        );
        let checkbox = found[2].fix.as_ref().unwrap();
        assert_eq!(checkbox.current, "Text me updates");
        assert!(checkbox.replacement.starts_with(
            "By checking this box, I agree to receive text messages from Acme \
                             Stone about customer care, account notifications and marketing \
                             offers."
        ));
        assert!(
            checkbox
                .replacement
                .contains("Consent is not a condition of purchase.")
        );

        // No checkbox at all next to a phone field.
        let pages = self::pages(
            "<form><input type=\"tel\" name=\"phone\"><a href=\"/privacy\">Privacy</a>\
             <a href=\"/terms\">Terms</a></form>",
            &format!("<h1>Acme Stone</h1>{FOOTER}"),
        );
        let found = facts(&draft(), &pages);
        assert_eq!(found.len(), 1);
        assert_eq!(
            found[0].fact,
            "The quote form asks for a phone number but has no checkbox to agree to texts."
        );
    }

    #[test]
    fn the_website_shows_the_registrations_contact_details() {
        let mut draft = draft();
        draft.brand.phone = "+18599405321".into();
        draft.brand.email = "dasha.gdindy@gmail.com".into();
        let home = "<h1>Acme Stone</h1><p>Call (317) 548-1060 or 317.548.1060, or \
                    <a href=\"mailto:info@acmestone.com\">info@acmestone.com</a>.</p>\
                    <img src=\"logo@2x.png\">";
        let pages = pages(&good_form().replace(FOOTER, ""), home);
        let found = facts(&draft, &pages);
        let fixes: Vec<(&str, &str)> = found
            .iter()
            .filter_map(|fact| fact.fix.as_ref())
            .map(|fix| (fix.location.as_str(), fix.replacement.as_str()))
            .collect();
        assert_eq!(
            fixes,
            [
                ("brand.phone", "+13175481060"),
                ("brand.email", "info@acmestone.com"),
                (
                    "page:website",
                    "Address: 100 Main St, Indianapolis, IN 46204"
                ),
            ]
        );
        assert_eq!(found[0].fix.as_ref().unwrap().label, "Business phone");

        // A website without any: they are to be added to it.
        let pages = self::pages(&good_form().replace(FOOTER, ""), "<h1>Acme Stone</h1>");
        let found = facts(&draft, &pages);
        assert_eq!(found.len(), 1);
        assert_eq!(
            found[0].fix.as_ref().unwrap().replacement,
            "Phone: (859) 940-5321\nEmail: dasha.gdindy@gmail.com\nAddress: 100 Main St, \
             Indianapolis, IN 46204"
        );
    }

    #[test]
    fn lead_generation_and_seo_are_left_to_the_ai_to_fix() {
        let home = format!("<h1>Acme Stone</h1><p>Website and SEO by Rank Fast.</p>{FOOTER}");
        let found = facts(&draft(), &pages(&good_form(), &home));
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].check, "website");
        assert!(found[0].fact.contains("\"SEO\""), "{}", found[0].fact);
        assert_eq!(found[0].fix, None);
    }
}
