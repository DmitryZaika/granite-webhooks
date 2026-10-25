//! Thumbs-down on a CRM manual answer.
//!
//! A corrected answer (and, for the super admins' review, the manual section
//! and change it suggests), then the user's approval or escalation saved to
//! `message_feedback`. Port of the CRM's `crmManualFeedback.server.ts`.
//!
//! The CRM also showed the model its own source files (route components it
//! picked by name) and facts read from them; the API does not ship the
//! CRM's source, so corrections rest on the manual, the user's access and
//! the canned answers.

use super::crm_manual::{
    self, ACCESS_RULES, Access, cloudtalk_credentials_answer, cloudtalk_phone_answer, normalize,
};
use crate::ai::{Caller, Chat, MINI_FALLBACK_MODEL, MINI_MODEL};
use crate::followups::js::{js_len, js_slice, trim};
use crate::re;
use crate::state::AppState;
use serde::{Deserialize, Serialize};
use std::time::Duration;

const CORRECTION_RULES: &str = r#"You are correcting a CRM manual answer that the user disliked.
1. CURRENT USER ACCESS is mandatory. Never describe a workflow this user cannot open.
2. VERIFIED UI FACTS and APPLICATION SOURCE CODE are the highest authority for what exists — but still filter by CURRENT USER ACCESS.
3. If VERIFIED UI FACTS list Create Company but platform superuser = no, do NOT give those steps. Say they need platform superuser access.
4. Never send the user to Admin > Company settings > Company when they asked about Superuser Dashboard Create Company and they are a platform superuser.
5. Be direct and actionable. Use **bold** for UI labels and numbered steps. Give the answer once — do not repeat the same steps twice.
6. Never mention source code, files, GitHub, documentation, or that you reviewed code.
7. Use only UI labels that appear in VERIFIED UI FACTS, code, or manual. Do NOT invent buttons, tabs, or pages.
8. Navigation paths: use sidebar labels like Superuser Dashboard > Companies — never raw URLs in the user-facing answer.
9. After your answer, add a final line with ONLY [[MANUAL_SECTION:section-id]] (prefer admin-portal-other-areas for Superuser Companies). If unsure, use [[MANUAL_SECTION:none]].
10. On the next line, add [[MANUAL_PATCH]] with real markdown steps (never placeholders like "replace the word here"). If no change needed, use [[MANUAL_PATCH:none]]."#;

/// A corrected answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Correction {
    pub answer: String,
    pub section_id: Option<String>,
    pub proposed_manual_change: Option<String>,
}

/// One message of the conversation the feedback is about.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, crate::openapi::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct DialogMessage {
    /// `user` or `assistant`.
    pub role: DialogRole,
    /// What was said.
    pub content: String,
    /// Which correction this is (1 or 2), for a corrected answer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub correction_attempt: Option<i64>,
    /// Whether the user liked this correction.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub correction_liked: Option<Option<bool>>,
}

/// Who said a dialog message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, crate::openapi::ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum DialogRole {
    User,
    Assistant,
}

fn is_rejected_code_backed_answer(answer: &str) -> bool {
    let n = normalize(answer);
    n.contains("cannot confirm")
        || n.contains("can't confirm")
        || n.contains("not described")
        || n.contains("not documented")
        || n.contains("there is no documented")
        || (n.contains("company settings") && (n.contains("admin") || n.contains("/admin/company")))
}

fn create_company_answer(field_list: &str) -> String {
    [
        "To add a company as a platform superuser:",
        "",
        "1. Open **Superuser Dashboard** → **Companies** in the sidebar.",
        "2. Click **Create Company**.",
        "3. Fill in the **Create Company** dialog and submit.",
        "",
        &format!("Dialog fields: {field_list}."),
        "",
        "This is separate from adding a customer with the **Company** toggle on **CRM > Customers**, and from **Admin > Company settings > Company** (active company settings only).",
    ]
    .join("\n")
}

fn no_superuser_company_answer(access: Option<&Access>) -> String {
    if access.is_some_and(|a| a.is_employee) {
        return [
            "Creating a platform company requires **platform superuser** access. Your account does not have it.",
            "",
            "If you meant a **customer** marked as a company:",
            "1. Open **CRM > Customers**.",
            "2. Click **Add Customer**.",
            "3. Turn on the **Company** toggle and enter the company name.",
            "4. Complete the required fields and submit.",
        ]
        .join("\n");
    }
    "Creating a platform company requires **platform superuser** access. Your account does not have it.".into()
}

fn asks_to_add_company(question: &str, customer_excluded: bool) -> bool {
    let n = normalize(question);
    n.contains("create company")
        || n.contains("add company")
        || n.contains("new company")
        || (n.contains("add")
            && n.contains("company")
            && (!customer_excluded || !n.contains("customer")))
}

/// `buildAnswerFromManual`: adding a company, from the manual's superuser pages.
fn answer_from_manual(manual: &str, question: &str, access: Option<&Access>) -> Option<String> {
    if !asks_to_add_company(question, true) {
        return None;
    }
    if access.is_some_and(|a| !a.is_platform_superuser) {
        return Some(no_superuser_company_answer(access));
    }
    let pages = crm_manual::section("admin-portal-other-areas")
        .or_else(|| crm_manual::section("cross-cutting-features"))
        .unwrap_or(manual);
    (pages.contains("Create Company") && pages.contains("Superuser Dashboard")).then(|| {
        create_company_answer(
            "Company Name, Subdomain, optional address, sales tax, domain, CloudTalk keys",
        )
    })
}

fn is_wrong_customer_as_company_answer(answer: &str, question: &str) -> bool {
    if !asks_to_add_company(question, false) {
        return false;
    }
    let n = normalize(answer);
    (n.contains("add customer") || n.contains("company toggle")) && !n.contains("create company")
}

/// The first half when the answer says the same steps twice.
fn dedupe_repeated_answer(answer: &str) -> String {
    let normalized = trim(answer);
    let len = js_len(normalized);
    let midpoint = len / 2;
    if midpoint < 80 {
        return normalized.to_string();
    }
    let first = js_slice(normalized, 0, midpoint);
    let second = js_slice(normalized, midpoint, len);
    let (first, second) = (trim(&first), trim(&second));
    if first.contains("Create Company")
        && second.contains("Create Company")
        && first.contains("Superuser Dashboard")
        && second.contains("Superuser Dashboard")
    {
        return first.to_string();
    }
    normalized.to_string()
}

/// `guessManualSectionId`: the section a question is about.
pub fn guess_section_id(question: &str) -> Option<String> {
    let n = normalize(question);
    if n.contains("superuser")
        || n.contains("create company")
        || n.contains("add company")
        || (n.contains("compan") && n.contains("dashboard"))
    {
        return Some("admin-portal-other-areas".into());
    }
    if n.contains("cloudtalk")
        && (n.contains("access key")
            || n.contains("access secret")
            || n.contains("credential")
            || n.contains("api key"))
    {
        return Some("admin-portal-other-areas".into());
    }
    if crm_manual::asks_cloudtalk_phone(&n) {
        return Some("admin-company-settings-users".into());
    }
    let ids = crm_manual::section_ids_for(question);
    ids.iter()
        .find(|id| !id.starts_with("how-the-application"))
        .or_else(|| ids.first())
        .cloned()
}

/// `guessManualSectionIdFromDialog`.
pub fn guess_section_from_dialog(dialog: &[DialogMessage]) -> Option<String> {
    let text: Vec<&str> = dialog
        .iter()
        .filter(|m| m.role == DialogRole::User)
        .map(|m| m.content.as_str())
        .collect();
    let text = text.join(" ");
    if text.is_empty() {
        None
    } else {
        guess_section_id(&text)
    }
}

fn sanitize(
    result: Correction,
    question: &str,
    manual: &str,
    access: Option<&Access>,
) -> Correction {
    let Correction {
        mut answer,
        mut section_id,
        mut proposed_manual_change,
    } = result;
    answer = dedupe_repeated_answer(&answer);
    if proposed_manual_change.as_deref().is_some_and(|p| {
        re!(
            r"replace the word here|todo|tbd|placeholder|\[\.\.\.\]",
            "i"
        )
        .is_match(p)
    }) {
        proposed_manual_change = None;
    }
    let n = normalize(&answer);
    if access.is_some_and(|a| !a.is_platform_superuser)
        && n.contains("create company")
        && n.contains("superuser dashboard")
    {
        return Correction {
            answer: no_superuser_company_answer(access),
            section_id: section_id.or_else(|| guess_section_id(question)),
            proposed_manual_change: None,
        };
    }
    let fallback = cloudtalk_phone_answer(question, access)
        .or_else(|| answer_from_manual(manual, question, access))
        .or_else(|| cloudtalk_credentials_answer(question, access));
    if let Some(fallback) = fallback
        && (is_rejected_code_backed_answer(&answer)
            || is_wrong_customer_as_company_answer(&answer, question))
    {
        answer = fallback;
        section_id = section_id.or_else(|| Some("admin-portal-other-areas".into()));
        if access.is_some_and(|a| a.is_platform_superuser) && proposed_manual_change.is_none() {
            proposed_manual_change = Some(
                [
                    "### Superuser Dashboard — Companies",
                    "- Sidebar: **Superuser Dashboard** → **Companies**",
                    "- Click **Create Company** to add a company (name, subdomain, optional address/tax/domain/CloudTalk)",
                ]
                .join("\n"),
            );
        }
    }
    Correction {
        answer: dedupe_repeated_answer(&answer),
        section_id: section_id.or_else(|| guess_section_id(question)),
        proposed_manual_change,
    }
}

/// `stripCorrectionMarkers`: the answer and its `[[MANUAL_SECTION]]` /
/// `[[MANUAL_PATCH]]` lines.
fn strip_markers(raw: &str) -> Correction {
    let mut answer = trim(raw).to_string();
    let mut proposed = None;
    if let Some(m) = re!(r"\[\[MANUAL_PATCH:none\]\]\s*$", "i").find(&answer) {
        answer = trim(&format!("{}{}", &answer[..m.start()], &answer[m.end()..])).to_string();
    } else if let Some(c) = re!(r"(?s)\[\[MANUAL_PATCH\]\]\s*(.*)$", "i").captures(&answer) {
        let value = trim(&c[1]).to_string();
        proposed = (!value.is_empty()).then_some(value);
        let whole = c.get(0).map(|m| m.range()).unwrap_or_default();
        answer = trim(&format!(
            "{}{}",
            &answer[..whole.start],
            &answer[whole.end..]
        ))
        .to_string();
    }
    let mut section_id = None;
    if let Some(c) = re!(r"\[\[MANUAL_SECTION:([^\]]+)\]\]\s*$", "i").captures(&answer) {
        let value = trim(&c[1]).to_string();
        section_id = (!value.eq_ignore_ascii_case("none")).then_some(value);
        let whole = c.get(0).map(|m| m.range()).unwrap_or_default();
        answer = trim(&format!(
            "{}{}",
            &answer[..whole.start],
            &answer[whole.end..]
        ))
        .to_string();
    }
    Correction {
        answer,
        section_id,
        proposed_manual_change: proposed,
    }
}

/// `generateCrmManualCorrection`: attempt 1 or 2 at a better answer.
pub async fn correction(
    state: &AppState,
    caller: Caller<'_>,
    question: &str,
    original_answer: &str,
    attempt: i64,
    previous: &[String],
    access: Option<&Access>,
) -> Correction {
    let manual = crm_manual::content_for(&state.pool, question).await;
    let quick = cloudtalk_phone_answer(question, access)
        .or_else(|| cloudtalk_credentials_answer(question, access))
        .or_else(|| answer_from_manual(&manual, question, access));
    if let Some(answer) = quick {
        return sanitize(
            Correction {
                answer,
                section_id: guess_section_id(question),
                proposed_manual_change: None,
            },
            question,
            &manual,
            access,
        );
    }
    let previous_attempts = if previous.is_empty() {
        String::new()
    } else {
        let list: Vec<String> = previous
            .iter()
            .enumerate()
            .map(|(i, a)| format!("{}. {a}", i + 1))
            .collect();
        format!(
            "Previous correction attempts the user also disliked:\n{}\n\n",
            list.join("\n\n")
        )
    };
    let access_block = access.map_or_else(String::new, |a| {
        format!("{}\n\n{ACCESS_RULES}\n\n", crm_manual::access_block(a))
    });
    let system = format!(
        "{access_block}APPLICATION SOURCE CODE: none matched this question.\n\nCRM USER MANUAL:\n\n{manual}\n\n{CORRECTION_RULES}"
    );
    let user = format!(
        "{previous_attempts}The user asked: {question}\n\nThe assistant answered:\n{original_answer}\n\nThe user disliked that answer. This is correction attempt {attempt} of 2. Provide one correct answer based on CURRENT USER ACCESS, then VERIFIED UI FACTS / source code, then the manual. Do not repeat the same steps twice."
    );
    let raw = model_correction(state, caller, &system, &user).await;
    let Some(raw) = raw.filter(|r| !r.is_empty()) else {
        let fallback = cloudtalk_phone_answer(question, access)
            .or_else(|| cloudtalk_credentials_answer(question, access))
            .or_else(|| answer_from_manual(&manual, question, access));
        return sanitize(
            Correction {
                answer: fallback.unwrap_or_else(|| {
                    "I could not generate a better answer right now. Please try again, or mark this as still not helpful so we can review it.".into()
                }),
                section_id: guess_section_id(question),
                proposed_manual_change: None,
            },
            question,
            &manual,
            access,
        );
    };
    let mut stripped = strip_markers(&raw);
    stripped.section_id = stripped.section_id.or_else(|| guess_section_id(question));
    sanitize(stripped, question, &manual, access)
}

/// The model's correction within 25 s, or none.
async fn model_correction(
    state: &AppState,
    caller: Caller<'_>,
    system: &str,
    user: &str,
) -> Option<String> {
    let run = async {
        let mut chat = Chat::new(MINI_MODEL)
            .message("system", system)
            .message("user", user)
            .max_tokens(Some(700));
        chat.timeout = Duration::from_secs(25);
        match state.ai.chat(&state.pool, caller, &chat).await {
            Err(err) if err.is_model_permission() => {
                chat.model = MINI_FALLBACK_MODEL;
                state.ai.chat(&state.pool, caller, &chat).await
            }
            other => other,
        }
    };
    tokio::time::timeout(Duration::from_secs(25), run)
        .await
        .ok()
        .and_then(Result::ok)
        .map(|text| trim(&text).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markers_are_read_off_the_end() {
        let c = strip_markers(
            "Do this.\n[[MANUAL_SECTION:employee-sms]]\n[[MANUAL_PATCH]]\n- step one",
        );
        assert_eq!(c.answer, "Do this.");
        assert_eq!(c.section_id.as_deref(), Some("employee-sms"));
        assert_eq!(c.proposed_manual_change.as_deref(), Some("- step one"));
        let none = strip_markers("Fine.\n[[MANUAL_SECTION:none]]\n[[MANUAL_PATCH:none]]");
        assert_eq!(
            (
                none.answer.as_str(),
                none.section_id,
                none.proposed_manual_change
            ),
            ("Fine.", None, None)
        );
    }

    #[test]
    fn sections_are_guessed_from_the_question() {
        assert_eq!(
            guess_section_id("how do I create company").as_deref(),
            Some("admin-portal-other-areas")
        );
        assert_eq!(
            guess_section_id("cloudtalk phone number").as_deref(),
            Some("admin-company-settings-users")
        );
        // Like the CRM: the first section after "how the application is organized".
        assert_eq!(
            guess_section_id("how do I mark an sms unread").as_deref(),
            Some("user-positions-access")
        );
    }

    #[test]
    fn a_non_superuser_never_gets_create_company_steps() {
        let access = Access {
            is_employee: true,
            is_admin: false,
            is_platform_superuser: false,
            position_labels: vec![],
        };
        let c = sanitize(
            Correction {
                answer: "Open Superuser Dashboard and click Create Company.".into(),
                section_id: None,
                proposed_manual_change: Some("TODO".into()),
            },
            "how do I add a company",
            "",
            Some(&access),
        );
        assert!(
            c.answer
                .starts_with("Creating a platform company requires **platform superuser** access.")
        );
        assert!(c.proposed_manual_change.is_none());
    }
}
