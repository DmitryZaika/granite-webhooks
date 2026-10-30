//! Who owns the next contact, and channel restrictions the customer asked
//! for (port of `smsFlowAiContactResume.ts`).

use super::js::trim;
use crate::{re, str_enum};
use regex::Regex;

str_enum! { pub enum ContactResumeMode {
    Date = "DATE", CustomerInitiates = "CUSTOMER_INITIATES", ManualReenable = "MANUAL_REENABLE", None = "NONE",
} }

str_enum! { pub enum AutomationContactStatus {
    Active = "ACTIVE", WaitForCustomer = "WAIT_FOR_CUSTOMER", Paused = "PAUSED", Stopped = "STOPPED",
} }

str_enum! { pub enum ChannelPermission {
    Allowed = "allowed", Restricted = "restricted", Unavailable = "unavailable", Unknown = "unknown",
} }

/// "I'll call you once the cabinets are in": the condition, squashed.
pub fn extract_customer_resume_condition(text: &str) -> Option<String> {
    let caps = re!(
        r"i('ll| will) (?:contact|call|reach out|text|co\s*tact|get back to).{0,40}(?:once|after|when)\s+(.{5,120}?)(?:\.|!|\?|$)",
        "i"
    )
    .captures(text)?;
    let condition = trim(caps.get(2)?.as_str());
    (!condition.is_empty()).then(|| re!(r"\s+").replace_all(condition, " ").into_owned())
}

fn customer_owns_next_contact_re() -> &'static Regex {
    re!(
        r"\bwe('ll| will) be in touch\b|\bi('ll| will) (?:get back (?:to you|with you)|reach back out|be in touch)\b|\bwe('ll| will) let you know\b|\bi('ll| will) call (?:you )?(?:once|when)\b|\bi('ll| will) contact you\b|\bwe('ll| will) contact you\b",
        "i"
    )
}

fn customer_initiates_re() -> &'static Regex {
    re!(
        r"i('ll| will) (text|contact|reach out|call|write|co\s*tact|get back to).{0,60}(once|after|when).{0,80}(estimate|ready|hear|back|done|them|from them)|i('ll| will) (text|contact|reach out|call|write).{0,40}when (i('m| am)|we('re| are)|ready)|wait for me to (reach out|text|contact)|she'?ll (text|write|contact) whenever|he'?ll (text|write|contact) whenever|she said she'?ll (write|text|contact)|he said he'?ll (write|text|contact)|whenever (she|he|i|we) (is|am|are) ready|i('ll| will) (text|contact|reach out) (you )?when|will contact you when\s*ever|contact you whenever|whenever i plan|i will contact you when|i will contact you once|will contact you once|contact you once|when we have time to visit|напиш(ет|у|ешь|ете|е)( сама)?[, ]*(как|когда|коли)|сказ(ала|ал) напиш|я свяжусь|сама свяжусь|я напишу|я зв.?яжус|напишу[, ]*когда",
        "i"
    )
}

/// "We'll be in touch": the customer will reach out next.
pub fn text_looks_like_customer_owns_next_contact(text: &str) -> bool {
    let trimmed = trim(text);
    if trimmed.is_empty()
        || re!(
            r"^(thanks|thank you|ty|ok|okay|got it|sounds good|understood)[.!]?\s*$",
            "i"
        )
        .is_match(trimmed)
    {
        return false;
    }
    customer_owns_next_contact_re().is_match(trimmed)
}

pub fn text_looks_like_customer_initiates(text: &str) -> bool {
    text_looks_like_customer_owns_next_contact(text) || customer_initiates_re().is_match(text)
}

pub fn text_looks_like_no_text_request(text: &str) -> bool {
    re!(
        r"don'?t text( me)?|do not text( me)?|asked to not text|not text (her|him|them)|stop texting|no (more )?texts|don'?t (sms|message) me",
        "i"
    )
    .is_match(text)
}

pub fn text_looks_like_email_preferred(text: &str) -> bool {
    re!(
        r"email (is |would be )?(better|fine|ok|okay|preferred)|prefer email|email me instead",
        "i"
    )
    .is_match(text)
}

/// How automation should resume, and which channels are off limits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
#[allow(clippy::struct_excessive_bools)]
pub struct ContactResume {
    pub contact_resume_mode: ContactResumeMode,
    pub automation_contact_status: Option<AutomationContactStatus>,
    pub sms_restricted: bool,
    pub email_restricted: bool,
    pub phone_restricted: bool,
}

pub fn infer_contact_resume(
    customer_and_note_text: &str,
    has_finite_resume_date: bool,
) -> ContactResume {
    let text = customer_and_note_text;
    let no_text = text_looks_like_no_text_request(text);
    let general_no_contact =
        re!(r"don'?t contact me|do not contact me|don'?t (call or text|text or call)|stop (all )?contact", "i").is_match(text);
    if text_looks_like_customer_initiates(text) || general_no_contact {
        return ContactResume {
            contact_resume_mode: ContactResumeMode::CustomerInitiates,
            automation_contact_status: Some(AutomationContactStatus::WaitForCustomer),
            sms_restricted: no_text || general_no_contact,
            email_restricted: general_no_contact,
            phone_restricted: general_no_contact,
        };
    }
    let open = ContactResume {
        contact_resume_mode: ContactResumeMode::None,
        automation_contact_status: None,
        sms_restricted: false,
        email_restricted: false,
        phone_restricted: false,
    };
    if no_text {
        return ContactResume {
            sms_restricted: true,
            ..open
        };
    }
    if has_finite_resume_date {
        return ContactResume {
            contact_resume_mode: ContactResumeMode::Date,
            automation_contact_status: Some(AutomationContactStatus::Paused),
            ..open
        };
    }
    open
}
