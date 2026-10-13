//! The email that carries signing links: the company's branded card with
//! the sender's message and a button per document. Port of the CRM's
//! `appendSignatureRequestLinksToBody` and `signatureInvitationHtml`.

use common::utils::email_body::{escape_html, is_html_markup, plain_email_to_html};
use std::fmt::Write as _;

const EMAIL_FONT: &str = "font-family:Inter,'Helvetica Neue',Arial,sans-serif;";

/// Who the invitation comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Branding {
    pub company_name: String,
    pub logo_url: Option<String>,
    pub sender_name: String,
}

/// A link a signer may be sent to: `SignWell`, or our own `/sign/` page.
pub fn is_allowed_signing_link(value: &str) -> bool {
    let Ok(url) = reqwest::Url::parse(value) else {
        return false;
    };
    if url.scheme() != "https" && url.scheme() != "http" {
        return false;
    }
    url.host_str() == Some("www.signwell.com") || url.path().starts_with("/sign/")
}

fn logo_html(branding: &Branding) -> String {
    let logo = branding
        .logo_url
        .as_deref()
        .map(str::trim)
        .filter(|url| url.to_lowercase().starts_with("https://"));
    logo.map_or_else(
        || {
            format!(
                "<div style=\"{EMAIL_FONT}font-size:22px;font-weight:700;color:#18181b;\">{}</div>",
                escape_html(&branding.company_name)
            )
        },
        |url| {
            format!(
                "<img src=\"{}\" alt=\"{}\" height=\"56\" style=\"display:block;height:56px;max-width:240px;border:0;margin:0 auto;\">",
                escape_html(url),
                escape_html(&branding.company_name)
            )
        },
    )
}

fn card_html(branding: &Branding, rows: &str) -> String {
    format!(
        "<table role=\"presentation\" width=\"100%\" cellpadding=\"0\" cellspacing=\"0\" style=\"background:#f4f4f5;\">
  <tr><td align=\"center\" style=\"padding:32px 12px;\">
    <table role=\"presentation\" width=\"100%\" cellpadding=\"0\" cellspacing=\"0\" style=\"max-width:600px;\">
      <tr><td style=\"background:#ffffff;border-top:4px solid #ffd400;border-radius:8px;padding:36px 40px;\">
        <table role=\"presentation\" width=\"100%\" cellpadding=\"0\" cellspacing=\"0\">
          <tr><td align=\"center\" style=\"padding-bottom:24px;\">{}</td></tr>
          {rows}
        </table>
      </td></tr>
      <tr><td align=\"center\" style=\"{EMAIL_FONT}font-size:12px;color:#a1a1aa;padding-top:16px;\">Sent by {}</td></tr>
    </table>
  </td></tr>
</table>",
        logo_html(branding),
        escape_html(&branding.company_name)
    )
}

fn button_html(href: &str, label: &str) -> String {
    format!(
        "<a href=\"{}\" style=\"{EMAIL_FONT}display:inline-block;background:#ffd400;color:#18181b;text-decoration:none;font-size:15px;font-weight:700;padding:14px 34px;border-radius:999px;\">{label}</a>",
        escape_html(href)
    )
}

fn invitation_html(body: &str, links: &[(&str, &str)], branding: &Branding) -> String {
    let trimmed = body.trim();
    let message = if trimmed.is_empty() {
        String::new()
    } else if is_html_markup(trimmed) {
        trimmed.to_string()
    } else {
        plain_email_to_html(trimmed)
    };
    let buttons = links
        .iter()
        .fold(String::new(), |mut out, (filename, url)| {
            let label = if links.len() == 1 {
                "View &amp; Complete Document".to_string()
            } else {
                format!("Review and sign {}", escape_html(filename))
            };
            let _ = write!(
                out,
                "<tr><td align=\"center\" style=\"padding:8px 0;\">\n        {}\n      </td></tr>",
                button_html(url, &label)
            );
            out
        });
    let message_row = if message.is_empty() {
        String::new()
    } else {
        format!(
            "<tr><td align=\"center\" style=\"background:#f4f4f5;border-radius:8px;padding:22px 26px;{EMAIL_FONT}font-size:15px;line-height:1.6;color:#3f3f46;text-align:center;\">{message}</td></tr>"
        )
    };
    card_html(
        branding,
        &format!(
            "<tr><td align=\"center\" style=\"{EMAIL_FONT}font-size:22px;color:#18181b;padding-bottom:28px;\">Please Review {}</td></tr>
          {message_row}
          <tr><td style=\"height:28px;line-height:28px;\">&nbsp;</td></tr>
          {buttons}",
            if links.len() == 1 { "This Document" } else { "These Documents" }
        ),
    )
}

/// The sender's notice that a signer opened, signed or declined a document
/// (`signatureStatusEmailHtml`): the branded card with the document, what
/// happened, the signer and a button to open it.
pub fn status_html(
    branding: &Branding,
    title: &str,
    document_name: &str,
    message: &str,
    signer_name: &str,
    status: &str,
    link: Option<(&str, &str)>,
) -> String {
    let initials: String = signer_name
        .split_whitespace()
        .take(2)
        .filter_map(|part| part.chars().next())
        .flat_map(char::to_uppercase)
        .collect();
    let initials = if initials.is_empty() {
        "?".to_string()
    } else {
        initials
    };
    let button = link.map_or_else(String::new, |(href, label)| {
        format!(
            "<tr><td align=\"center\" style=\"padding:4px 0;\">{}</td></tr>",
            button_html(href, &escape_html(label))
        )
    });
    card_html(
        branding,
        &format!(
            "<tr><td align=\"center\" style=\"{EMAIL_FONT}font-size:24px;color:#18181b;padding-bottom:6px;\">{}</td></tr>
          <tr><td align=\"center\" style=\"{EMAIL_FONT}font-size:17px;font-weight:600;color:#3f3f46;padding-bottom:28px;\">{}</td></tr>
          <tr><td style=\"{EMAIL_FONT}font-size:15px;line-height:1.6;color:#3f3f46;padding-bottom:28px;\">{}</td></tr>
          <tr><td align=\"center\" style=\"padding-bottom:28px;\">
            <table role=\"presentation\" cellpadding=\"0\" cellspacing=\"0\"><tr>
              <td style=\"width:48px;height:48px;border-radius:999px;background:#ffd400;{EMAIL_FONT}font-size:16px;font-weight:700;color:#18181b;text-align:center;vertical-align:middle;\">{}</td>
              <td style=\"padding-left:14px;{EMAIL_FONT}\">
                <div style=\"font-size:16px;color:#18181b;\">{}</div>
                <div style=\"font-size:14px;color:#71717a;\">{}</div>
              </td>
            </tr></table>
          </td></tr>
          {button}",
            escape_html(title),
            escape_html(document_name),
            escape_html(message),
            escape_html(&initials),
            escape_html(signer_name),
            escape_html(status),
        ),
    )
}

/// The body with the signing links: the branded card, or (without
/// branding) a link per document after the message.
pub fn with_signing_links(
    body: &str,
    documents: &[(String, Option<String>)],
    branding: Option<&Branding>,
) -> String {
    let links: Vec<(&str, &str)> = documents
        .iter()
        .filter_map(|(filename, url)| {
            url.as_deref()
                .filter(|url| is_allowed_signing_link(url))
                .map(|url| (filename.as_str(), url))
        })
        .collect();
    if links.is_empty() {
        return body.to_string();
    }
    if let Some(branding) = branding {
        return invitation_html(body, &links, branding);
    }
    let block = links
        .iter()
        .fold(String::new(), |mut out, (filename, url)| {
            let _ = write!(
                out,
                "<p><a href=\"{}\">Review and sign {}</a></p>",
                escape_html(url),
                escape_html(filename)
            );
            out
        });
    let trimmed = body.trim();
    if trimmed.is_empty() {
        block
    } else if is_html_markup(trimmed) {
        format!("{trimmed}{block}")
    } else {
        format!("{}{block}", plain_email_to_html(trimmed))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_signing_pages_are_linked() {
        assert!(is_allowed_signing_link("https://www.signwell.com/docs/1"));
        assert!(is_allowed_signing_link(
            "https://sign.acme.test/sign/abc?token=x"
        ));
        assert!(!is_allowed_signing_link("https://evil.example/x"));
        assert!(!is_allowed_signing_link("javascript:alert(1)"));
    }

    #[test]
    fn one_document_gets_one_button_in_the_card() {
        let branding = Branding {
            company_name: "Acme <Stone>".into(),
            logo_url: None,
            sender_name: "Rita".into(),
        };
        let html = with_signing_links(
            "Please sign",
            &[(
                "contract.pdf".into(),
                Some("https://www.signwell.com/d/1".into()),
            )],
            Some(&branding),
        );
        assert!(html.contains("Please Review This Document"));
        assert!(html.contains("View &amp; Complete Document"));
        assert!(html.contains("href=\"https://www.signwell.com/d/1\""));
        assert!(html.contains("Sent by Acme &lt;Stone&gt;"));
        assert!(html.contains("<p>Please sign</p>"));
        let plain = with_signing_links(
            "<p>Hi</p>",
            &[("a.pdf".into(), Some("https://www.signwell.com/d/2".into()))],
            None,
        );
        assert_eq!(
            plain,
            "<p>Hi</p><p><a href=\"https://www.signwell.com/d/2\">Review and sign a.pdf</a></p>"
        );
        assert_eq!(
            with_signing_links("Hi", &[("a.pdf".into(), None)], None),
            "Hi"
        );
    }
}
