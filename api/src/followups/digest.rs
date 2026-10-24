//! The 9 AM "waiting on you" email: each rep's open deals whose customer
//! has had no text or email for a week and has no follow-up running,
//! customers who replied to a follow-up and went quiet again included. The
//! rep starts a follow-up from the deal (Generate with AI); nothing is sent
//! to customers. Monday to Saturday, once a day per rep
//! (`followup_digest_sends` is the claim), a few reps per job run.

use crate::auth::session::find_user;
use crate::state::AppState;
use crate::webhooks_client::OutgoingEmail;
use chrono::{Datelike, NaiveDateTime, Timelike, Weekday};
use chrono_tz::America::Indiana::Indianapolis;
use common::utils::email_body::escape_html;
use lambda_http::tracing;

use super::dates::Instant;

const DIGEST_HOUR: u32 = 9;
/// No text or email either way for this long.
pub const QUIET_DAYS: i64 = 7;
/// Rows in one email.
const MAX_ROWS: usize = 15;
/// Reps a job run sends to (the job runs every few minutes).
const REPS_PER_RUN: i64 = 5;

/// One deal waiting on the rep.
#[derive(sqlx::FromRow, Debug, Clone, PartialEq, Eq)]
pub struct QuietDeal {
    pub deal_id: i64,
    pub customer: String,
    pub list_name: String,
    /// The rep's (or a flow's) last text or email to them.
    pub last_out: Option<NaiveDateTime>,
    /// Their last text or email to the company.
    pub last_in: Option<NaiveDateTime>,
    /// A follow-up of theirs once stopped because they replied.
    pub replied_to_followup: bool,
}

impl QuietDeal {
    fn last(&self) -> Option<NaiveDateTime> {
        self.last_out.max(self.last_in)
    }

    /// They wrote last: an answer is owed.
    fn waiting_for_reply(&self) -> bool {
        self.last_in
            .is_some_and(|inbound| self.last_out.is_none_or(|out| inbound > out))
    }

    /// The email line after the name.
    pub fn status(&self, now: NaiveDateTime) -> String {
        let days = |at: NaiveDateTime| (now - at).num_days().max(0);
        match self.last() {
            None => "no text or email yet".into(),
            Some(_) if self.waiting_for_reply() => format!(
                "they wrote last, {} days ago",
                days(self.last_in.unwrap_or(now))
            ),
            Some(last) if self.replied_to_followup => {
                format!("replied to a follow-up, then quiet for {} days", days(last))
            }
            Some(last) => format!("last message {} days ago", days(last)),
        }
    }
}

/// Owed answers first, then customers who replied before, then the
/// warmest (most recent contact; never contacted last).
pub fn rank(mut deals: Vec<QuietDeal>) -> Vec<QuietDeal> {
    deals.sort_by(|a, b| {
        b.waiting_for_reply()
            .cmp(&a.waiting_for_reply())
            .then(b.replied_to_followup.cmp(&a.replied_to_followup))
            .then(b.last().cmp(&a.last()))
            .then(a.deal_id.cmp(&b.deal_id))
    });
    deals
}

/// The rep's quiet open deals.
pub async fn quiet_deals(
    state: &AppState,
    user_id: i32,
    company_id: i32,
    now: Instant,
) -> Result<Vec<QuietDeal>, sqlx::Error> {
    let phone = "RIGHT(REGEXP_REPLACE(COALESCE(c.phone, ''), '[^0-9]', ''), 10)";
    let last_sms = |table: &str, direction: &str| {
        format!(
            "(SELECT MAX(s.created_date) FROM {table} s
               WHERE s.company_id = c.company_id AND s.is_echo = 0 AND s.phone_digits = {phone}
                 AND s.direction = '{direction}')"
        )
    };
    let sql = format!(
        "SELECT deal_id, customer, list_name, last_out, last_in, replied_to_followup FROM (
           SELECT CAST(d.id AS SIGNED) AS deal_id, COALESCE(NULLIF(TRIM(c.name), ''), 'Customer') AS customer,
                  COALESCE(l.name, '') AS list_name,
                  CAST(NULLIF(GREATEST(COALESCE({ct_out}, '1970-01-01'), COALESCE({rc_out}, '1970-01-01'),
                                       COALESCE({tx_out}, '1970-01-01'),
                                       COALESCE((SELECT MAX(e.sent_at) FROM email_participants ep
                                                   JOIN emails e ON e.id = ep.email_id AND e.deleted_at IS NULL
                                                  WHERE ep.customer_id = c.id AND e.sender_user_id IS NOT NULL
                                                    AND e.is_draft = 0), '1970-01-01')),
                              '1970-01-01') AS DATETIME) AS last_out,
                  CAST(NULLIF(GREATEST(COALESCE({ct_in}, '1970-01-01'), COALESCE({rc_in}, '1970-01-01'),
                                       COALESCE({tx_in}, '1970-01-01'),
                                       COALESCE((SELECT MAX(e.sent_at) FROM email_participants ep
                                                   JOIN emails e ON e.id = ep.email_id AND e.deleted_at IS NULL
                                                  WHERE ep.customer_id = c.id AND ep.type = 'from'
                                                    AND e.sender_user_id IS NULL), '1970-01-01')),
                              '1970-01-01') AS DATETIME) AS last_in,
                  EXISTS (SELECT 1 FROM sms_flow_enrollments f
                           WHERE f.customer_id = c.id AND f.status = 'stopped_by_reply') AS replied_to_followup
             FROM deals d
             JOIN customers c ON c.id = d.customer_id AND c.company_id = ? AND c.deleted_at IS NULL
             JOIN deals_list l ON l.id = d.list_id AND l.deleted_at IS NULL
            WHERE d.user_id = ? AND d.deleted_at IS NULL AND d.is_won IS NULL
              AND d.created_at < ? - INTERVAL 1 DAY
              AND LOWER(l.name) NOT LIKE '%hold%'
              AND NOT EXISTS (SELECT 1 FROM sms_flow_enrollments f
                               WHERE f.customer_id = c.id AND f.status IN ('active', 'paused'))
              AND (COALESCE(TRIM(c.phone), '') <> ''
                   OR EXISTS (SELECT 1 FROM customers_emails ce WHERE ce.customer_id = c.id))
         ) quiet
         WHERE COALESCE(GREATEST(COALESCE(last_out, '1970-01-01'), COALESCE(last_in, '1970-01-01')), '1970-01-01')
               < ? - INTERVAL {QUIET_DAYS} DAY
         LIMIT 200",
        ct_out = last_sms("cloudtalk_sms", "outbound"),
        rc_out = last_sms("ringcentral_sms", "outbound"),
        tx_out = last_sms("telnyx_sms", "outbound"),
        ct_in = last_sms("cloudtalk_sms", "inbound"),
        rc_in = last_sms("ringcentral_sms", "inbound"),
        tx_in = last_sms("telnyx_sms", "inbound"),
    );
    let deals: Vec<QuietDeal> = sqlx::query_as(sqlx::AssertSqlSafe(sql))
        .bind(company_id)
        .bind(user_id)
        .bind(now.naive_utc())
        .bind(now.naive_utc())
        .fetch_all(&state.pool)
        .await?;
    Ok(rank(deals))
}

/// The email's subject and HTML.
pub fn compose(app_url: &str, deals: &[QuietDeal], now: NaiveDateTime) -> (String, String) {
    let count = deals.len();
    let subject = if count == 1 {
        "1 customer is waiting on you".to_string()
    } else {
        format!("{count} customers are waiting on you")
    };
    let mut html = format!(
        "<p>These customers have had no text or email for {QUIET_DAYS}+ days, and no follow-up is running:</p><ul>"
    );
    for deal in deals.iter().take(MAX_ROWS) {
        html.push_str(&format!(
            "<li><a href=\"{}\">{}</a> · {} · {}</li>",
            escape_html(&format!(
                "{app_url}/employee/deals/edit/{}/project",
                deal.deal_id
            )),
            escape_html(&deal.customer),
            escape_html(deal.list_name.trim()),
            escape_html(&deal.status(now)),
        ));
    }
    html.push_str("</ul>");
    if count > MAX_ROWS {
        html.push_str(&format!("<p>And {} more.</p>", count - MAX_ROWS));
    }
    html.push_str(
        "<p>Open one and click <b>Generate with AI</b> under Automated Follow-up to start a follow-up.</p>",
    );
    (subject, html)
}

/// At 9 AM store time (not Sundays), emails up to a few reps who have
/// quiet deals and no email today; how many were sent.
pub async fn send_quiet_digests(state: &AppState, now: Instant) -> Result<usize, sqlx::Error> {
    let local = now.with_timezone(&Indianapolis);
    if local.hour() != DIGEST_HOUR || local.weekday() == Weekday::Sun {
        return Ok(0);
    }
    let day = local.date_naive();
    let reps: Vec<(i32, i32)> = sqlx::query_as(
        "SELECT DISTINCT d.user_id, c.company_id FROM deals d
           JOIN customers c ON c.id = d.customer_id AND c.deleted_at IS NULL
           JOIN users u ON u.id = d.user_id AND u.company_id = c.company_id AND u.is_deleted = 0
          WHERE d.deleted_at IS NULL AND d.is_won IS NULL AND d.user_id IS NOT NULL
            AND NOT EXISTS (SELECT 1 FROM followup_digest_sends s
                             WHERE s.user_id = d.user_id AND s.sent_on = ?)
          ORDER BY d.user_id LIMIT ?",
    )
    .bind(day)
    .bind(REPS_PER_RUN)
    .fetch_all(&state.pool)
    .await?;
    let mut sent = 0;
    for (user_id, company_id) in reps {
        // The (user, day) row is the claim: a second run sends nothing.
        let claimed = sqlx::query(
            "INSERT IGNORE INTO followup_digest_sends (user_id, company_id, sent_on, sent_at)
             VALUES (?, ?, ?, UTC_TIMESTAMP())",
        )
        .bind(user_id)
        .bind(company_id)
        .bind(day)
        .execute(&state.pool)
        .await?
        .rows_affected()
            == 1;
        if !claimed {
            continue;
        }
        let deals = quiet_deals(state, user_id, company_id, now).await?;
        if deals.is_empty() {
            continue;
        }
        let Some(rep) = find_user(&state.pool, user_id, company_id).await? else {
            continue;
        };
        if rep.email.trim().is_empty() {
            continue;
        }
        let company: Option<Option<String>> =
            sqlx::query_scalar("SELECT name FROM company WHERE id = ?")
                .bind(company_id)
                .fetch_optional(&state.pool)
                .await?;
        let company = company.flatten().unwrap_or_default();
        let (subject, body) = compose(&state.app_url, &deals, now.naive_utc());
        let email = OutgoingEmail {
            from_name: Some(format!("{} follow-ups", company.trim())),
            from_address: common::amazon::email::DEFAULT_NOREPLY_EMAIL_ADDRESS.to_string(),
            to: vec![rep.email.trim().to_string()],
            cc: Vec::new(),
            bcc: Vec::new(),
            subject,
            body,
            signature: None,
            reply_to: Vec::new(),
            in_reply_to: None,
            references: Vec::new(),
            attachments: Vec::new(),
            transactional: true,
        };
        match state.webhooks.send_email(&email).await {
            Ok(_) => sent += 1,
            Err(err) => tracing::warn!(?err, user_id, "follow-up digest not sent"),
        }
    }
    Ok(sent)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(day: u32) -> NaiveDateTime {
        chrono::NaiveDate::from_ymd_opt(2026, 10, day)
            .unwrap()
            .and_hms_opt(12, 0, 0)
            .unwrap()
    }

    fn deal(id: i64, out: Option<u32>, inbound: Option<u32>, replied: bool) -> QuietDeal {
        QuietDeal {
            deal_id: id,
            customer: format!("Customer {id}"),
            list_name: "Got a Quote".into(),
            last_out: out.map(at),
            last_in: inbound.map(at),
            replied_to_followup: replied,
        }
    }

    #[test]
    fn owed_answers_come_first_then_replies_then_the_warmest() {
        let ranked = rank(vec![
            deal(1, None, None, false),
            deal(2, Some(10), None, false),
            deal(3, Some(5), Some(8), false),
            deal(4, Some(12), None, false),
            deal(5, Some(9), Some(6), true),
        ]);
        let ids: Vec<i64> = ranked.iter().map(|d| d.deal_id).collect();
        assert_eq!(ids, [3, 5, 4, 2, 1]);
        let now = at(24);
        assert_eq!(ranked[0].status(now), "they wrote last, 16 days ago");
        assert_eq!(
            ranked[1].status(now),
            "replied to a follow-up, then quiet for 15 days"
        );
        assert_eq!(ranked[2].status(now), "last message 12 days ago");
        assert_eq!(ranked[4].status(now), "no text or email yet");
        let (subject, html) = compose("https://app.test", &ranked, now);
        assert_eq!(subject, "5 customers are waiting on you");
        assert!(html.contains("<a href=\"https://app.test/employee/deals/edit/3/project\">Customer 3</a> · Got a Quote · they wrote last, 16 days ago"), "{html}");
        let many: Vec<QuietDeal> = (1..=20).map(|id| deal(id, Some(10), None, false)).collect();
        assert!(
            compose("https://app.test", &many, now)
                .1
                .contains("And 5 more.")
        );
    }
}
