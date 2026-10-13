//! The CRM user manual in the chat.
//!
//! The sections a question needs (with the super admins' corrections from
//! `crm_manual_overrides`), what the asker may open, canned answers, and
//! when a question is about using the CRM. Ports of `crmManual.server.ts`, `crmManualUserAccess.server.ts`,
//! `crmManualHowToRedirect.ts` and `getCrmManualQuickAnswer`.
//!
//! `crm_manual.json` is `docs/manual/*.md` of the CRM repo plus the topic
//! table of `crmManual.server.ts`; `scripts/sync-crm-manual.mjs` rebuilds it.

use super::instructions::is_price_query;
use crate::auth::session::SessionUser;
use crate::followups::js::trim;
use crate::re;
use serde::Deserialize;
use sqlx::MySqlPool;
use std::collections::HashMap;
use std::sync::LazyLock;

#[derive(Deserialize)]
struct Section {
    id: String,
    content: String,
}

#[derive(Deserialize)]
struct Topic {
    id: String,
    keywords: Vec<String>,
    #[serde(default)]
    related: Vec<String>,
}

#[derive(Deserialize)]
struct Manual {
    intro: String,
    sections: Vec<Section>,
    core: Vec<String>,
    topics: Vec<Topic>,
    fallback: Vec<String>,
}

static MANUAL: LazyLock<Manual> = LazyLock::new(|| {
    serde_json::from_str(include_str!("crm_manual.json")).expect("crm_manual.json is valid")
});

/// The manual's section ids, in file order.
pub fn section_ids() -> Vec<&'static str> {
    MANUAL.sections.iter().map(|s| s.id.as_str()).collect()
}

/// A section's text as shipped (no override).
pub fn section(id: &str) -> Option<&'static str> {
    MANUAL
        .sections
        .iter()
        .find(|s| s.id == id)
        .map(|s| s.content.as_str())
}

fn normalize(text: &str) -> String {
    trim(&re!(r"\s+").replace_all(&text.to_lowercase(), " ")).to_string()
}

/// Whole word, or a phrase as a substring.
fn matches_keyword(text: &str, keyword: &str) -> bool {
    let keyword = keyword.to_lowercase();
    if keyword.contains(' ') {
        return text.contains(&keyword);
    }
    let word = |b: Option<&u8>| b.is_some_and(|b| b.is_ascii_alphanumeric() || *b == b'_');
    let bytes = text.as_bytes();
    let starts = word(keyword.as_bytes().first());
    let ends = word(keyword.as_bytes().last());
    text.match_indices(&keyword).any(|(at, _)| {
        word(at.checked_sub(1).and_then(|i| bytes.get(i))) != starts
            && word(bytes.get(at + keyword.len())) != ends
    })
}

/// `getCrmManualSectionIdsForQuery`.
pub fn section_ids_for(search: &str) -> Vec<String> {
    let normalized = normalize(search);
    let mut selected: Vec<String> = MANUAL.core.clone();
    let add = |id: &str, selected: &mut Vec<String>| {
        if !selected.iter().any(|s| s == id) {
            selected.push(id.to_string());
        }
    };
    let mut matched = false;
    for topic in &MANUAL.topics {
        if topic
            .keywords
            .iter()
            .any(|k| matches_keyword(&normalized, k))
        {
            matched = true;
            add(&topic.id, &mut selected);
            for related in &topic.related {
                add(related, &mut selected);
            }
        }
    }
    if !matched {
        for id in &MANUAL.fallback {
            add(id, &mut selected);
        }
    }
    selected
}

/// Super admins' corrections of sections (empty before the table exists).
pub async fn overrides(pool: &MySqlPool) -> HashMap<String, String> {
    sqlx::query_as::<_, (String, String)>("SELECT section_id, content FROM crm_manual_overrides")
        .fetch_all(pool)
        .await
        .map(|rows| rows.into_iter().collect())
        .unwrap_or_default()
}

fn assemble(ids: &[String], overrides: &HashMap<String, String>) -> String {
    let core = &MANUAL.core;
    let mut ordered: Vec<&String> = core.iter().filter(|id| ids.contains(id)).collect();
    ordered.extend(ids.iter().filter(|id| !core.contains(id)));
    let mut seen = std::collections::HashSet::new();
    let mut parts = vec![MANUAL.intro.clone()];
    for id in ordered {
        if !seen.insert(id) {
            continue;
        }
        if let Some(text) = overrides
            .get(id)
            .map(String::as_str)
            .or_else(|| section(id))
        {
            parts.push(text.to_string());
        }
    }
    parts.join("\n\n---\n\n")
}

/// `loadCrmManualContent`: the sections the text is about.
pub async fn content_for(pool: &MySqlPool, search: &str) -> String {
    assemble(&section_ids_for(search), &overrides(pool).await)
}

/// What the asker may open, for the manual's answers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Access {
    pub is_employee: bool,
    pub is_admin: bool,
    pub is_platform_superuser: bool,
    pub position_labels: Vec<String>,
}

fn position_label(id: i32) -> String {
    match id {
        1 => "Sales Rep".into(),
        2 => "Sales Manager".into(),
        3 => "Shop Manager".into(),
        4 => "Shop Worker".into(),
        5 => "Manager".into(),
        6 => "Installer".into(),
        7 => "Marketing".into(),
        8 => "Walk-in".into(),
        9 => "Super Admin".into(),
        10 => "Office Manager".into(),
        11 => "Template Guy".into(),
        other => format!("Position {other}"),
    }
}

/// `loadCrmManualUserAccess`.
pub async fn access(pool: &MySqlPool, user: &SessionUser) -> sqlx::Result<Access> {
    let positions: Vec<i32> = sqlx::query_scalar(
        "SELECT DISTINCT position_id FROM users_positions WHERE user_id = ? AND company_id = ?",
    )
    .bind(user.id)
    .bind(user.company_id)
    .fetch_all(pool)
    .await?;
    Ok(Access {
        is_employee: user.is_employee,
        is_admin: user.is_admin,
        is_platform_superuser: user.is_superuser,
        position_labels: positions.into_iter().map(position_label).collect(),
    })
}

/// `formatCrmManualUserAccessBlock`.
pub fn access_block(access: &Access) -> String {
    let yes = |b: bool| if b { "yes" } else { "no" };
    let positions = if access.position_labels.is_empty() {
        "none".to_string()
    } else {
        access.position_labels.join(", ")
    };
    format!(
        "CURRENT USER ACCESS (strict — only describe UI this user can open):\n- Employee portal: {}\n- Admin portal: {}\n- Platform superuser (Superuser Dashboard / Create Company): {}\n- Positions at current company: {positions}",
        yes(access.is_employee),
        yes(access.is_admin),
        yes(access.is_platform_superuser)
    )
}

pub const ACCESS_RULES: &str = r#"Access rules for the current user:
1. Only describe pages, buttons, and workflows this user can open with CURRENT USER ACCESS above.
2. If they ask about a feature they cannot access, say clearly that they do not have access and which role is required. Do not give step-by-step instructions for locked features.
3. Superuser Dashboard and Create Company require platform superuser = yes. Super Admin position alone is not enough.
4. Admin portal pages require Admin portal = yes. Company settings (Users / Company tax/logo) also need the Manager position.
5. Shop Worker only has Shop Transactions, Samples, My Account — not employee CRM or admin.
6. Marketing position is the external marketing portal only — not employee CRM or admin.
7. If a non-superuser asks how to "add a new company", do NOT describe Superuser Dashboard. If they have Employee portal access, you may describe CRM > Customers → Add Customer → Company toggle (customer marked as a company). Otherwise say they lack access.
8. Never invent access the user does not have."#;

/// `looksLikeCrmManualHowToQuery`: a question about using the CRM itself.
pub fn looks_like_how_to(query: &str) -> bool {
    let normalized = normalize(query);
    if normalized.is_empty() || is_price_query(&normalized) {
        return false;
    }
    let help = [
        re!(r"\bhow (?:do|can|should|would|does)\b"),
        re!(r"\bhow to\b"),
        re!(r"\bwhere (?:do|can|is|are)\b"),
        re!(r"\bwhere can i find\b"),
        re!(r"\bcan i\b"),
        re!(r"\bhelp(?: me)?\b"),
        re!(r"\bneed to\b"),
        re!(r"\bwant to\b"),
        re!(r"\btrying to\b"),
        re!(r"\bwalk me through\b"),
    ]
    .iter()
    .any(|p| p.is_match(&normalized));
    let product = [
        re!(r"\bcrm\b"),
        re!(r"\bgranite manager\b"),
        re!(r"\bgranitemanager\b"),
        re!(r"\bin the app\b"),
        re!(r"\bin this app\b"),
        re!(r"\bthis software\b"),
        re!(r"\bthe software\b"),
        re!(r"\bin the system\b"),
        re!(r"\bemployee portal\b"),
        re!(r"\badmin portal\b"),
        re!(r"\buser manual\b"),
    ]
    .iter()
    .any(|p| p.is_match(&normalized));
    if product && help {
        return true;
    }
    if re!(r"\b(?:add|create|edit|delete|upload|change|open|find|use|set up|setup|sell|remove|update|enable|disable|mark|reply|compose|attach|restore|undelete)\s+(?:a |an |the |new )?(?:stone|stones|slab|slabs|customer|customers|deal|deals|user|users|sink|sinks|faucet|faucets|supplier|suppliers|invoice|invoices|checklist|checklists|instruction|instructions|logo|company|email|emails|questionnaire|questionnaires|note|notes|event|events|message|messages|inbox|sms|thread|conversation|draft|drafts)\b").is_match(&normalized) {
        return true;
    }
    if help
        && re!(r"\b(?:inbox|mailbox|unread|mark as unread|mark as read|mark unread|mark read|new email|email thread|email draft|sms|text message|text messages|deal board|activity panel|customer information|find customer|drafts folder|drafts|attach files|ai reply|improve text)\b").is_match(&normalized)
    {
        return true;
    }
    re!(r"\bwhere (?:is|are|do i find|can i find)\b.+\b(?:page|button|sidebar|menu|tab|setting|screen|form|folder|inbox)\b").is_match(&normalized)
}

fn asks_cloudtalk_phone(normalized: &str) -> bool {
    (normalized.contains("cloudtalk") || normalized.contains("qualtalk"))
        && ["phone", "number", "line", "sms sender", "outbound"]
            .iter()
            .any(|w| normalized.contains(w))
}

fn can_edit_admin_users(access: Option<&Access>) -> bool {
    access.is_some_and(|a| {
        a.is_platform_superuser || (a.is_admin && a.position_labels.iter().any(|l| l == "Manager"))
    })
}

fn cloudtalk_phone_answer(question: &str, access: Option<&Access>) -> Option<String> {
    let normalized = normalize(question);
    if !asks_cloudtalk_phone(&normalized) {
        return None;
    }
    let asks_where = ["find", "get", "where", "obtain", "look up"]
        .iter()
        .any(|w| normalized.contains(w));
    let find_steps = [
        "The phone number itself lives in CloudTalk, not in Granite Manager.",
        "",
        "1. Log in to the [CloudTalk Dashboard](https://dashboard.cloudtalk.io) with **CloudTalk Admin** access.",
        "2. Open **Numbers** and search for the line assigned to the user.",
        "   You can also open **Users → Edit user** and check which numbers are assigned.",
        "3. Copy the full number including country code.",
    ]
    .join("\n");
    if asks_where
        && !["crm", "granite", "enter", "set"]
            .iter()
            .any(|w| normalized.contains(w))
    {
        return Some(find_steps);
    }
    let mut parts: Vec<String> = vec![
        "**CloudTalk phone number** and **CloudTalk agent ID** are separate fields in Granite Manager.".into(),
        String::new(),
        "### Find the CloudTalk phone number".into(),
        String::new(),
        find_steps,
    ];
    if can_edit_admin_users(access) {
        parts.extend([
            String::new(),
            "### Enter it for another user".into(),
            String::new(),
            [
                "1. Open **Admin → Company settings → Users**.",
                "2. Click **Edit** on the user.",
                "3. Paste the number into **CloudTalk phone number** (at least 10 digits).",
                "4. Submit.",
            ]
            .join("\n"),
        ]);
    }
    if access.is_some_and(|a| a.is_employee) {
        parts.extend([
            String::new(),
            "### Enter it on your own account".into(),
            String::new(),
            [
                "1. Open **My Account** in the sidebar.",
                "2. Paste the number into **CloudTalk phone number**.",
                "3. Save.",
            ]
            .join("\n"),
        ]);
    }
    if access.is_some() && !can_edit_admin_users(access) && access.is_some_and(|a| a.is_employee) {
        parts.extend([
            String::new(),
            "To set another user's CloudTalk phone number, a **Manager** with **Admin** access must use **Admin → Company settings → Users → Edit**.".into(),
        ]);
    }
    parts.extend([
        String::new(),
        "**CloudTalk phone number** is the outbound SMS line Granite Manager uses for that user. **CloudTalk agent ID** links call history and **Made a call** activity.".into(),
    ]);
    Some(parts.join("\n"))
}

fn cloudtalk_credentials_answer(question: &str, access: Option<&Access>) -> Option<String> {
    let n = normalize(question);
    let asks = n.contains("cloudtalk")
        && (n.contains("access key")
            || n.contains("access secret")
            || n.contains("credential")
            || n.contains("api key")
            || n.contains("api keys")
            || (n.contains("key") && n.contains("secret"))
            || n.contains("where to get")
            || n.contains("how do i get"));
    if !asks {
        return None;
    }
    if access.is_some_and(|a| !a.is_platform_superuser) {
        return Some(
            [
                "CloudTalk API credentials are set when a **platform superuser** creates or edits a company in Granite Manager.",
                "",
                "Your account does not have platform superuser access. Ask a platform superuser to add or update the company CloudTalk keys.",
            ]
            .join("\n"),
        );
    }
    Some(
        [
            "**CloudTalk Access Key** and **CloudTalk Access Secret** are pasted into Granite Manager, but you obtain them from CloudTalk — not from Granite Manager.",
            "",
            "### Get the keys in CloudTalk",
            "",
            "1. Log in to the [CloudTalk Dashboard](https://dashboard.cloudtalk.io) with **CloudTalk Admin** access.",
            "2. Go to **Account → Settings → API Keys**.",
            "3. Click **+ Add API Key** (or use an existing key).",
            "4. Copy the **Access Key ID** and **Access Key Secret** (use **Show** to reveal the secret).",
            "",
            "### Enter them in Granite Manager",
            "",
            "1. Open **Superuser Dashboard → Companies**.",
            "2. Click **Create Company** (or edit an existing company).",
            "3. Paste the values into **CloudTalk Access Key** and **CloudTalk Access Secret**.",
            "4. Submit the form.",
        ]
        .join("\n"),
    )
}

/// `getCrmManualQuickAnswer`: a canned answer for questions the manual gets wrong.
pub fn quick_answer(question: &str, access: Option<&Access>) -> Option<String> {
    cloudtalk_phone_answer(question, access)
        .or_else(|| cloudtalk_credentials_answer(question, access))
}
