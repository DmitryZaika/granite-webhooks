//! Tells the rep when a follow-up ends without them: the customer got back
//! to them (answer now), or it failed (fix it and start it again). A bell
//! notification on the customer's deal and an email, once per run
//! (`sms_flow_enrollments.ended_alert_at` is the claim).

use crate::auth::session::find_user;
use crate::state::AppState;
use crate::webhooks_client::OutgoingEmail;
use chrono::NaiveDateTime;
use common::utils::email_body::escape_html;
use lambda_http::tracing;
use sqlx::MySqlPool;

/// Ended runs alerted per job run.
const ALERT_BATCH: i64 = 20;
/// `notifications.notification_type` of a reply to a follow-up.
pub const FOLLOWUP_REPLIED: &str = "followup_replied";
/// `notifications.notification_type` of a follow-up that failed.
pub const FOLLOWUP_FAILED: &str = "followup_failed";
const EXCERPT_CHARS: usize = 140;

#[derive(sqlx::FromRow, Debug)]
struct Ended {
    id: i32,
    company_id: i32,
    user_id: i32,
    customer_id: Option<i32>,
    phone_digits: Option<String>,
    customer_email: Option<String>,
    status: String,
    error_message: Option<String>,
    started_at: NaiveDateTime,
}

/// What the rep is told about one ended run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Alert {
    pub kind: &'static str,
    /// The bell line (255 characters at most).
    pub message: String,
    pub subject: String,
    /// Plain-text lines of the email.
    pub lines: Vec<String>,
}

/// Why a run failed, for the rep.
pub fn failure_reason(token: Option<&str>) -> String {
    match token.unwrap_or("").trim() {
        "agent_not_linked" => "your texting line isn't linked to your account".into(),
        "sms_not_configured" => "texting isn't set up for your company".into(),
        "invalid_phone" => "the customer's number can't receive texts".into(),
        "unfilled_variables" => "a step has a blank only you can fill in".into(),
        "flow_step_invalid" => "a step is empty, or the customer has no email address".into(),
        "invalid_email" | "send_failed" => "the email couldn't be sent".into(),
        "ai_generation_failed" => "the AI couldn't write the message".into(),
        "" => "of an error".into(),
        other => format!("the texting provider refused it ({other})"),
    }
}

fn excerpt(text: &str) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= EXCERPT_CHARS {
        flat
    } else {
        format!("{}…", flat.chars().take(EXCERPT_CHARS).collect::<String>())
    }
}

fn clip(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        text.to_string()
    } else {
        format!("{}…", text.chars().take(max - 1).collect::<String>())
    }
}

/// The alert for a run that ended by `status` (`stopped_by_reply` or
/// `failed`), its error token, and the customer's last message since the
/// start, if any.
pub fn compose(status: &str, error: Option<&str>, customer: &str, reply: Option<&str>) -> Alert {
    if status == "failed" {
        let reason = failure_reason(error);
        return Alert {
            kind: FOLLOWUP_FAILED,
            message: clip(
                &format!("Follow-up stopped because {reason}. Fix it, then start it again."),
                255,
            ),
            subject: clip(&format!("Follow-up for {customer} stopped"), 200),
            lines: vec![
                format!("The follow-up for {customer} stopped because {reason}."),
                "Fix it, then start the follow-up again from the customer's deal.".into(),
            ],
        };
    }
    let (message, lines) = match reply.map(excerpt).filter(|r| !r.is_empty()) {
        Some(reply) => (
            format!("Replied to your follow-up: “{reply}”"),
            vec![
                format!("{customer} replied to your follow-up:"),
                format!("“{reply}”"),
                "Reply while it's fresh. AI reply in the conversation can draft it for you.".into(),
            ],
        ),
        None => (
            "Got back to you during a follow-up. Answer while it's fresh.".to_string(),
            vec![
                format!("{customer} got back to you while a follow-up was running."),
                "Reply while it's fresh. AI reply in the conversation can draft it for you.".into(),
            ],
        ),
    };
    Alert {
        kind: FOLLOWUP_REPLIED,
        message: clip(&message, 255),
        subject: clip(&format!("{customer} replied to your follow-up"), 200),
        lines,
    }
}

fn phone10(raw: &str) -> Option<String> {
    let digits: String = raw.chars().filter(char::is_ascii_digit).collect();
    (digits.len() >= 10).then(|| digits[digits.len() - 10..].to_string())
}

/// The customer's last text or email to the company since `since`.
async fn last_reply(pool: &MySqlPool, run: &Ended) -> Result<Option<String>, sqlx::Error> {
    let mut latest: Option<(NaiveDateTime, String)> = None;
    if let Some(phone) = run.phone_digits.as_deref().and_then(phone10) {
        for table in ["cloudtalk_sms", "ringcentral_sms", "telnyx_sms"] {
            let sql = format!(
                "SELECT CAST(created_date AS DATETIME), text FROM {table}
                  WHERE company_id = ? AND is_echo = 0 AND phone_digits = ? AND direction = 'inbound'
                    AND created_date > ?
                  ORDER BY created_date DESC, id DESC LIMIT 1"
            );
            let row: Option<(NaiveDateTime, String)> = sqlx::query_as(sqlx::AssertSqlSafe(sql))
                .bind(run.company_id)
                .bind(&phone)
                .bind(run.started_at)
                .fetch_optional(pool)
                .await?;
            latest = latest.max(row);
        }
    }
    if let Some(customer_id) = run.customer_id {
        let row: Option<(NaiveDateTime, String)> = sqlx::query_as(
            "SELECT CAST(e.sent_at AS DATETIME), e.body FROM emails e
               JOIN email_participants ep ON ep.email_id = e.id AND ep.customer_id = ? AND ep.type = 'from'
              WHERE e.company_id = ? AND e.sender_user_id IS NULL AND e.deleted_at IS NULL AND e.sent_at > ?
              ORDER BY e.sent_at DESC, e.id DESC LIMIT 1",
        )
        .bind(customer_id)
        .bind(run.company_id)
        .bind(run.started_at)
        .fetch_optional(pool)
        .await?;
        let row = row.map(|(at, body)| (at, super::gather::strip_html_tags(&body)));
        latest = latest.max(row);
    }
    Ok(latest.map(|(_, text)| text))
}

/// The customer's name and their latest deal (open first).
async fn customer_and_deal(
    pool: &MySqlPool,
    run: &Ended,
) -> Result<(String, Option<i64>), sqlx::Error> {
    let fallback = run
        .phone_digits
        .clone()
        .or_else(|| run.customer_email.clone())
        .unwrap_or_else(|| "A customer".into());
    let Some(customer_id) = run.customer_id else {
        return Ok((fallback, None));
    };
    let name: Option<Option<String>> = sqlx::query_scalar(
        "SELECT name FROM customers WHERE id = ? AND company_id = ? AND deleted_at IS NULL",
    )
    .bind(customer_id)
    .bind(run.company_id)
    .fetch_optional(pool)
    .await?;
    let deal: Option<i64> = sqlx::query_scalar(
        "SELECT CAST(id AS SIGNED) FROM deals WHERE customer_id = ? AND deleted_at IS NULL
          ORDER BY is_won IS NULL DESC, id DESC LIMIT 1",
    )
    .bind(customer_id)
    .fetch_optional(pool)
    .await?;
    let name = name
        .flatten()
        .map(|n| n.trim().to_string())
        .filter(|n| !n.is_empty())
        .unwrap_or(fallback);
    Ok((name, deal))
}

async fn send(state: &AppState, run: &Ended) -> Result<(), String> {
    let pool = &state.pool;
    let err = |e: sqlx::Error| e.to_string();
    let (customer, deal_id) = customer_and_deal(pool, run).await.map_err(err)?;
    let reply = if run.status == "stopped_by_reply" {
        last_reply(pool, run).await.map_err(err)?
    } else {
        None
    };
    let alert = compose(
        &run.status,
        run.error_message.as_deref(),
        &customer,
        reply.as_deref(),
    );
    // The bell lists notifications on a live deal only.
    if let Some(deal_id) = deal_id {
        sqlx::query(
            "INSERT INTO notifications (user_id, customer_id, deal_id, message, notification_type, due_at)
             VALUES (?, ?, ?, ?, ?, UTC_TIMESTAMP())",
        )
        .bind(run.user_id)
        .bind(run.customer_id)
        .bind(deal_id)
        .bind(&alert.message)
        .bind(alert.kind)
        .execute(pool)
        .await
        .map_err(err)?;
    }
    let Some(rep) = find_user(pool, run.user_id, run.company_id)
        .await
        .map_err(err)?
    else {
        return Ok(());
    };
    if rep.email.trim().is_empty() {
        return Ok(());
    }
    let branding = crate::routes::emails::signatures::branding(state, &rep)
        .await
        .map_err(|e| format!("{e:?}"))?;
    let link = deal_id.map(|deal| format!("{}/employee/deals/edit/{deal}/project", state.app_url));
    let mut html: String = alert
        .lines
        .iter()
        .map(|line| format!("<p>{}</p>", escape_html(line)))
        .collect();
    if let Some(link) = &link {
        html.push_str(&format!(
            "<p><a href=\"{}\">Open {}</a></p>",
            escape_html(link),
            escape_html(&customer)
        ));
    }
    let email = OutgoingEmail {
        from_name: Some(format!("{} follow-ups", branding.company_name)),
        from_address: common::amazon::email::DEFAULT_NOREPLY_EMAIL_ADDRESS.to_string(),
        to: vec![rep.email.trim().to_string()],
        cc: Vec::new(),
        bcc: Vec::new(),
        subject: alert.subject,
        body: html,
        signature: None,
        reply_to: Vec::new(),
        in_reply_to: None,
        references: Vec::new(),
        attachments: Vec::new(),
        transactional: true,
    };
    state
        .webhooks
        .send_email(&email)
        .await
        .map(|_| ())
        .map_err(|e| format!("{e:?}"))
}

/// Alerts the reps of runs that recently ended by a reply or a failure;
/// how many were alerted. Each run is claimed first, so it is told once.
pub async fn alert_ended_followups(state: &AppState) -> Result<usize, sqlx::Error> {
    let ended: Vec<Ended> = sqlx::query_as(
        "SELECT id, company_id, user_id, customer_id, CAST(customer_phone_digits AS CHAR) AS phone_digits,
                customer_email, CAST(status AS CHAR) AS status, error_message,
                CAST(started_at AS DATETIME) AS started_at
           FROM sms_flow_enrollments
          WHERE status IN ('stopped_by_reply', 'failed') AND ended_alert_at IS NULL
            AND updated_at > UTC_TIMESTAMP() - INTERVAL 2 DAY
          ORDER BY id LIMIT ?",
    )
    .bind(ALERT_BATCH)
    .fetch_all(&state.pool)
    .await?;
    let mut alerted = 0;
    for run in ended {
        let claimed = sqlx::query(
            "UPDATE sms_flow_enrollments SET ended_alert_at = UTC_TIMESTAMP(), updated_at = updated_at
              WHERE id = ? AND ended_alert_at IS NULL",
        )
        .bind(run.id)
        .execute(&state.pool)
        .await?
        .rows_affected()
            == 1;
        if !claimed {
            continue;
        }
        match send(state, &run).await {
            Ok(()) => alerted += 1,
            Err(err) => tracing::warn!(err, enrollment = run.id, "follow-up end alert not sent"),
        }
    }
    Ok(alerted)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_rep_reads_what_happened_and_what_to_do() {
        let replied = compose(
            "stopped_by_reply",
            None,
            "Paula Prospect",
            Some(" Not now,\n maybe in May "),
        );
        assert_eq!(replied.kind, FOLLOWUP_REPLIED);
        assert_eq!(
            replied.message,
            "Replied to your follow-up: “Not now, maybe in May”"
        );
        assert_eq!(replied.subject, "Paula Prospect replied to your follow-up");
        let called = compose("stopped_by_reply", None, "Paula Prospect", None);
        assert!(called.message.starts_with("Got back to you"));
        let failed = compose("failed", Some("agent_not_linked"), "Paula Prospect", None);
        assert_eq!(failed.kind, FOLLOWUP_FAILED);
        assert_eq!(
            failed.message,
            "Follow-up stopped because your texting line isn't linked to your account. Fix it, then start it again."
        );
        assert_eq!(failed.subject, "Follow-up for Paula Prospect stopped");
        assert_eq!(
            failure_reason(Some("ringcentral_401")),
            "the texting provider refused it (ringcentral_401)"
        );
        let long = compose("stopped_by_reply", None, "P", Some(&"word ".repeat(80)));
        assert!(long.message.chars().count() <= 255);
        assert!(long.message.ends_with("…”"));
    }
}
