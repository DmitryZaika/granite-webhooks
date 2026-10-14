//! How a rep writes, from what they actually sent: the AI drafts (reply
//! button, follow-ups) show these as style examples so the copy sounds like
//! the rep, not like AI.

use super::js::{js_len, js_slice, trim};
use crate::re;
use sqlx::MySqlPool;

/// Recent sent texts read per SMS table before choosing examples.
const SMS_CANDIDATES: i64 = 25;
const EMAIL_CANDIDATES: i64 = 4;
const EDIT_CANDIDATES: i64 = 6;
pub const MAX_EXAMPLES: usize = 10;
const EXAMPLE_MAX_CHARS: usize = 320;
const MAX_CHARS: usize = 3_000;

/// How the rep writes, from their own sent messages.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RepVoice {
    /// The rep's recent messages, newest first (to any customer).
    pub examples: Vec<String>,
    /// The rep's email signature is added to every email they send.
    pub has_email_signature: bool,
    /// What the rep's emails go out as (`users.email_name`).
    pub email_name: Option<String>,
}

/// One line of plain text: tags dropped, entities decoded, whitespace
/// squashed, capped at [`EXAMPLE_MAX_CHARS`].
fn clean_example(text: &str) -> String {
    let no_tags = re!(r"<(?:br|/p|/div)[^>]*>", "gi").replace_all(text, "\n");
    let no_tags = re!(r"<[^>]*>", "g").replace_all(&no_tags, "");
    let decoded = no_tags
        .replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .replace("&#39;", "'")
        .replace("&quot;", "\"");
    let squashed = re!(r"\s+", "g").replace_all(&decoded, " ");
    let text = trim(&squashed);
    if js_len(text) > EXAMPLE_MAX_CHARS {
        format!("{}…", js_slice(text, 0, EXAMPLE_MAX_CHARS))
    } else {
        text.to_string()
    }
}

/// Up to [`MAX_EXAMPLES`] distinct examples, rep-edited ones first, within
/// [`MAX_CHARS`].
pub fn pick_voice_examples(edited: &[String], sent: &[String]) -> Vec<String> {
    let mut picked: Vec<String> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut total = 0;
    for raw in edited.iter().chain(sent) {
        let text = clean_example(raw);
        // Too short to show a voice ("ok", "Thanks!").
        if js_len(&text) < 12 || !seen.insert(text.to_lowercase()) {
            continue;
        }
        if total + js_len(&text) > MAX_CHARS {
            break;
        }
        total += js_len(&text);
        picked.push(text);
        if picked.len() >= MAX_EXAMPLES {
            break;
        }
    }
    picked
}

/// The prompt block of the rep's messages; empty when there are none.
pub fn format_voice_block(examples: &[String]) -> String {
    if examples.is_empty() {
        return String::new();
    }
    format!(
        "RECENT MESSAGES BY THE REP (to various customers, newest first). Write like this: same length, wording, punctuation, and how they open and end. Never copy their content:\n{}",
        examples
            .iter()
            .map(|t| format!("- {t}"))
            .collect::<Vec<_>>()
            .join("\n")
    )
}

/// The rep's recent messages: what they sent after editing AI suggestions,
/// then their own texts and emails (not ones the app sent automatically).
pub async fn load_rep_voice(
    pool: &MySqlPool,
    company_id: i32,
    user_id: i32,
) -> Result<RepVoice, sqlx::Error> {
    let edited: Vec<String> = sqlx::query_scalar(
        "SELECT final_text FROM ai_followup_edit_pairs
          WHERE user_id = ? AND company_id = ? ORDER BY id DESC LIMIT ?",
    )
    .bind(user_id)
    .bind(company_id)
    .bind(EDIT_CANDIDATES)
    .fetch_all(pool)
    .await?;
    let mut sent: Vec<(String, chrono::NaiveDateTime)> = Vec::new();
    for table in ["cloudtalk_sms", "ringcentral_sms"] {
        let sql = format!(
            "SELECT text, CAST(created_date AS DATETIME) AS created_date FROM {table}
              WHERE company_id = ? AND sender_user_id = ? AND direction = 'outbound'
                AND status = 'sent' AND is_automated = 0 AND text IS NOT NULL
              ORDER BY created_date DESC, id DESC LIMIT ?"
        );
        let rows: Vec<(String, chrono::NaiveDateTime)> = sqlx::query_as(sqlx::AssertSqlSafe(sql))
            .bind(company_id)
            .bind(user_id)
            .bind(SMS_CANDIDATES)
            .fetch_all(pool)
            .await?;
        sent.extend(rows);
    }
    let emails: Vec<(String, chrono::NaiveDateTime)> = sqlx::query_as(
        "SELECT body, CAST(sent_at AS DATETIME) AS sent_at FROM emails
          WHERE sender_user_id = ? AND company_id = ? AND deleted_at IS NULL AND sent_at IS NOT NULL
            AND is_draft = 0 AND is_automated = 0
          ORDER BY sent_at DESC, id DESC LIMIT ?",
    )
    .bind(user_id)
    .bind(company_id)
    .bind(EMAIL_CANDIDATES)
    .fetch_all(pool)
    .await?;
    sent.extend(emails);
    sent.sort_by(|a, b| b.1.cmp(&a.1));
    let sent: Vec<String> = sent.into_iter().map(|(text, _)| text).collect();
    let profile: Option<(Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT email_signature, email_name FROM users WHERE id = ? AND company_id = ?",
    )
    .bind(user_id)
    .bind(company_id)
    .fetch_optional(pool)
    .await?;
    let (signature, email_name) = profile.unwrap_or_default();
    Ok(RepVoice {
        examples: pick_voice_examples(&edited, &sent),
        has_email_signature: signature.is_some_and(|s| !clean_example(&s).is_empty()),
        email_name: email_name
            .map(|n| trim(&n).to_string())
            .filter(|n| !n.is_empty()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn voice_examples_are_cleaned_deduped_and_capped() {
        let edited = vec!["Hey Ann, I can bring samples Friday".to_string()];
        let sent = vec![
            "ok".to_string(),
            "hey ann, i can bring samples friday".to_string(),
            "<p>Sounds good&nbsp;- I'll have the quote over by 5.</p><p>Dema</p>".to_string(),
        ];
        let picked = pick_voice_examples(&edited, &sent);
        assert_eq!(
            picked,
            [
                "Hey Ann, I can bring samples Friday",
                "Sounds good - I'll have the quote over by 5. Dema"
            ]
        );
        let many: Vec<String> = (0..30)
            .map(|i| format!("Message number {i} here"))
            .collect();
        assert_eq!(pick_voice_examples(&[], &many).len(), MAX_EXAMPLES);
    }

    #[test]
    fn the_block_lists_examples_or_is_empty() {
        assert_eq!(format_voice_block(&[]), "");
        let block = format_voice_block(&["Slabs are in, want to see them Thursday?".into()]);
        assert!(block.starts_with("RECENT MESSAGES BY THE REP"));
        assert!(block.ends_with("\n- Slabs are in, want to see them Thursday?"));
    }
}
