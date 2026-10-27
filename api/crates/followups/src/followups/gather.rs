//! What a follow-up is written from, loaded from the database.
//!
//! The customer's profile and deals, SMS and email history, templates,
//! instructions, holds and the rep's style, then the contact pressure and the
//! decision whether to write at all (port of `gatherFollowupContext` in
//! `smsFlowAiFollowup.server.ts`).

use super::cadence::types::FollowupSituation;
use super::crm_hold::{
    ActivityLike, CrmHoldUntil, NoteLike, crm_hold_from_notes, parse_followup_timeline_at,
    resolve_crm_hold_until,
};
use super::dates::Instant;
use super::js::{js_len, js_slice, trim};
use super::lifecycle::{
    FollowupGenerationDecision, GenerationDecisionInputs, TargetDealTerminal,
    evaluate_followup_generation_decision,
};
use super::pressure::build::{PressureInputs, build_followup_contact_pressure};
use super::pressure::repair::prepare_followup_validation;
use super::pressure::repair::{ValidationRepair, ValidationStatus};
use super::pressure::{
    DealState, FollowupContactPressure, FollowupTerminalDeal, OtherDealSummary, TargetDealSummary,
    TerminalStatus,
};
use super::prompt::{
    FollowupTemplateExample, FollowupTimelineMessage, TemplateScope, classify_followup_situation,
};
use super::send_window::FollowupChannel;
use super::style::{FollowupStyleProfile, parse_followup_style_profile};
use super::timeline::Direction;
use crate::re;
use chrono::NaiveDateTime;
use sqlx::MySqlPool;

const MAX_SMS: usize = 1000;
const MAX_EMAILS: usize = 1000;
const MAX_MSG_CHARS: usize = 4000;
const MAX_TEMPLATE_BODY_CHARS: usize = 600;
const MAX_TEMPLATES_PER_BUCKET: i64 = 8;
const MAX_INSTRUCTIONS_CHARS: usize = 12_000;
const MAX_PROFILE_CHARS: usize = 8_000;
const MAX_TOTAL_CONTEXT_CHARS: usize = 80_000;

/// Who a follow-up is for and what to write.
#[derive(Debug, Clone)]
pub struct FollowupRequest {
    pub company_id: i32,
    pub user_id: i32,
    pub customer_id: Option<i32>,
    pub phone_digits: Option<String>,
    pub customer_email: Option<String>,
    pub channel: FollowupChannel,
    pub step_position: usize,
    pub preferred_deal_id: Option<i64>,
    pub explicit_reenroll: bool,
    /// The rep overrode a soft "do not write" in the picker.
    pub force_generate: bool,
    pub now: Option<Instant>,
}

impl FollowupRequest {
    pub const fn new(company_id: i32, user_id: i32, channel: FollowupChannel) -> Self {
        Self {
            company_id,
            user_id,
            customer_id: None,
            phone_digits: None,
            customer_email: None,
            channel,
            step_position: 1,
            preferred_deal_id: None,
            explicit_reenroll: false,
            force_generate: false,
            now: None,
        }
    }
}

/// What a follow-up is written from.
#[derive(Debug, Clone)]
pub struct GatheredContext {
    pub customer_id: Option<i32>,
    pub situation: FollowupSituation,
    pub profile: String,
    pub timeline: Vec<FollowupTimelineMessage>,
    pub templates: Vec<FollowupTemplateExample>,
    pub company_instructions: String,
    pub admin_instruction: String,
    pub crm_hold: Option<CrmHoldUntil>,
    pub crm_note_hold: Option<CrmHoldUntil>,
    pub contact_pressure: FollowupContactPressure,
    pub generation_decision: FollowupGenerationDecision,
    pub style_profile: Option<FollowupStyleProfile>,
    /// The rep's own recent messages, for writing in their voice.
    pub rep_voice: super::rep_voice::RepVoice,
    pub validation_status: ValidationStatus,
    pub validation_repairs: Vec<ValidationRepair>,
    pub sms_loaded: usize,
    pub emails_loaded: usize,
}

impl GatheredContext {
    /// The rep chose Generate anyway: no hold, write.
    #[must_use]
    pub fn for_forced_generate(&self) -> Self {
        let mut forced = self.clone();
        forced.generation_decision = FollowupGenerationDecision::Generate;
        forced.contact_pressure =
            super::pressure::build::contact_pressure_for_forced_generate(&self.contact_pressure);
        forced
    }

    /// The later of the activity and note holds.
    pub fn crm_hold_for_prompt(&self) -> Option<CrmHoldUntil> {
        super::crm_hold::later_crm_hold(self.crm_hold.clone(), self.crm_note_hold.clone())
    }

    /// The date sends wait for: CRM holds or the day the customer asked for.
    pub fn hold_until(&self) -> Option<Instant> {
        super::crm_hold::effective_hold_until(&[
            self.crm_hold.as_ref().map(|h| h.until),
            self.crm_note_hold.as_ref().map(|h| h.until),
            self.contact_pressure.customer_requested_followup_at,
        ])
    }
}

fn truncate_text(text: &str, max: usize) -> String {
    let trimmed = trim(text);
    if js_len(trimmed) <= max {
        trimmed.to_string()
    } else {
        format!("{}…", js_slice(trimmed, 0, max))
    }
}

/// `new Date(value).toISOString().slice(0, 16)` with a space; the value as
/// it is when it is not a date.
fn format_when(value: &str) -> String {
    super::dates::js_date_parse(value)
        .and_then(super::dates::from_millis)
        .map_or_else(
            || value.to_string(),
            |at| super::dates::iso(at)[..16].replace('T', " "),
        )
}

fn format_db_time(at: NaiveDateTime) -> String {
    at.format("%Y-%m-%d %H:%M").to_string()
}

pub fn strip_html_tags(html: &str) -> String {
    re!(r"<[^>]*>", "g").replace_all(html, "").into_owned()
}

fn push_line(lines: &mut Vec<String>, label: &str, value: Option<&str>) {
    if let Some(value) = value.map(trim).filter(|v| !v.is_empty()) {
        lines.push(format!("{label}: {value}"));
    }
}

/// The profile text and the deal state the pressure reads.
struct CustomerProfile {
    profile: String,
    crm_hold: Option<CrmHoldUntil>,
    crm_note_hold: Option<CrmHoldUntil>,
    terminal_deal: Option<FollowupTerminalDeal>,
    target_deal: Option<TargetDealSummary>,
    other_deals: Vec<OtherDealSummary>,
}

#[derive(sqlx::FromRow)]
struct CoreRow {
    name: Option<String>,
    company_name: Option<String>,
    phone: Option<String>,
    phone_2: Option<String>,
    address: Option<String>,
    postal_code: Option<String>,
    source: Option<String>,
    sales_rep_name: Option<String>,
}

#[derive(sqlx::FromRow)]
struct ExtraRow {
    city: Option<String>,
    state: Option<String>,
    remodal_type: Option<String>,
    project_size: Option<String>,
    when_start: Option<String>,
    details: Option<String>,
    your_message: Option<String>,
    notes: Option<String>,
    backsplash: Option<String>,
    kitchen_stove: Option<String>,
    sink: Option<String>,
    remove_and_dispose: Option<String>,
    contact_time: Option<String>,
    created_date: Option<NaiveDateTime>,
}

#[derive(sqlx::FromRow)]
struct DealRow {
    id: i64,
    amount: Option<String>,
    lost_reason: Option<String>,
    list_name: Option<String>,
    is_won: Option<i8>,
    updated_at: Option<NaiveDateTime>,
}

#[allow(clippy::too_many_lines)]
async fn load_customer_profile(
    pool: &MySqlPool,
    customer_id: i32,
    company_id: i32,
    preferred_deal_id: Option<i64>,
    clock: Instant,
) -> Result<CustomerProfile, sqlx::Error> {
    let core: Option<CoreRow> = sqlx::query_as(
        "SELECT c.name, c.company_name, c.phone, c.phone_2, c.address, CAST(c.postal_code AS CHAR) AS postal_code,
                c.source, u.name AS sales_rep_name
           FROM customers c
           LEFT JOIN users u ON u.id = c.sales_rep AND u.is_deleted = 0
          WHERE c.id = ? AND c.company_id = ? AND c.deleted_at IS NULL",
    )
    .bind(customer_id)
    .bind(company_id)
    .fetch_optional(pool)
    .await?;
    let extra: Option<ExtraRow> = sqlx::query_as(
        "SELECT city, state, remodal_type, CAST(project_size AS CHAR) AS project_size, CAST(when_start AS CHAR) AS when_start,
                details, your_message, notes, CAST(backsplash AS CHAR) AS backsplash,
                CAST(kitchen_stove AS CHAR) AS kitchen_stove, CAST(sink AS CHAR) AS sink,
                CAST(remove_and_dispose AS CHAR) AS remove_and_dispose, CAST(contact_time AS CHAR) AS contact_time,
                CAST(created_date AS DATETIME) AS created_date
           FROM customers WHERE id = ? AND company_id = ? AND deleted_at IS NULL",
    )
    .bind(customer_id)
    .bind(company_id)
    .fetch_optional(pool)
    .await?;
    let deals: Vec<DealRow> = sqlx::query_as(
        "SELECT CAST(d.id AS SIGNED) AS id, CAST(d.amount AS CHAR) AS amount, d.lost_reason, l.name AS list_name,
                CAST(d.is_won AS SIGNED) AS is_won, CAST(d.updated_at AS DATETIME) AS updated_at
           FROM deals d JOIN deals_list l ON l.id = d.list_id
           JOIN customers c ON c.id = d.customer_id AND c.company_id = ?
          WHERE d.customer_id = ? AND d.deleted_at IS NULL AND l.deleted_at IS NULL
          ORDER BY d.id DESC LIMIT 8",
    )
    .bind(company_id)
    .bind(customer_id)
    .fetch_all(pool)
    .await?;

    let mut lines: Vec<String> = Vec::new();
    if let Some(core) = &core {
        push_line(&mut lines, "Name", core.name.as_deref());
        push_line(&mut lines, "Company", core.company_name.as_deref());
        push_line(&mut lines, "Phone", core.phone.as_deref());
        push_line(&mut lines, "Phone 2", core.phone_2.as_deref());
        push_line(&mut lines, "Address", core.address.as_deref());
        push_line(&mut lines, "Postal code", core.postal_code.as_deref());
        push_line(&mut lines, "Source", core.source.as_deref());
        push_line(
            &mut lines,
            "You (the salesperson sending this — write as I, never use this name)",
            core.sales_rep_name.as_deref(),
        );
    }
    if let Some(extra) = &extra {
        push_line(&mut lines, "City", extra.city.as_deref());
        push_line(&mut lines, "State", extra.state.as_deref());
        push_line(&mut lines, "Project type", extra.remodal_type.as_deref());
        push_line(&mut lines, "Project size", extra.project_size.as_deref());
        push_line(
            &mut lines,
            "When they want to start",
            extra.when_start.as_deref(),
        );
        push_line(
            &mut lines,
            "Best time to contact them",
            extra.contact_time.as_deref(),
        );
        push_line(&mut lines, "Sink", extra.sink.as_deref());
        push_line(&mut lines, "Backsplash", extra.backsplash.as_deref());
        push_line(&mut lines, "Stove", extra.kitchen_stove.as_deref());
        push_line(
            &mut lines,
            "Remove and dispose",
            extra.remove_and_dispose.as_deref(),
        );
        push_line(&mut lines, "Lead details", extra.details.as_deref());
        push_line(
            &mut lines,
            "Customer message",
            extra.your_message.as_deref(),
        );
        push_line(&mut lines, "Notes", extra.notes.as_deref());
        if let Some(created) = extra.created_date {
            lines.push(format!("Became a lead: {}", format_db_time(created)));
        }
    }
    let target_id = preferred_deal_id
        .and_then(|id| deals.iter().find(|d| d.id == id))
        .or_else(|| deals.iter().find(|d| d.is_won.is_none()))
        .or_else(|| deals.first())
        .map(|d| d.id);
    let mut other_deals = Vec::new();
    let mut terminal_deal = None;
    let mut target_deal = None;
    for deal in &deals {
        let won_label = match deal.is_won {
            Some(1) => Some("Closed/Won"),
            Some(0) => Some("Closed/Lost"),
            _ => None,
        };
        let status_label = deal
            .list_name
            .as_deref()
            .map(trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("Deal")
            .to_string();
        let is_target = target_id == Some(deal.id);
        let mut bits = vec![
            if is_target {
                format!("Target #{}", deal.id)
            } else {
                format!("Other #{}", deal.id)
            },
            status_label.clone(),
        ];
        if let Some(amount) = &deal.amount {
            // mysql2 returns DECIMAL as its text ("9999.00").
            bits.push(format!("amount {amount}"));
        }
        if let Some(label) = won_label {
            bits.push(label.to_string());
        }
        if let Some(reason) = deal
            .lost_reason
            .as_deref()
            .map(trim)
            .filter(|r| !r.is_empty())
        {
            bits.push(format!("lost: {reason}"));
        }
        lines.push(format!("Deal: {}", bits.join(" · ")));
        let label = won_label.map_or_else(
            || status_label.clone(),
            |won| format!("{status_label} · {won}"),
        );
        let terminal = matches!(deal.is_won, Some(0 | 1));
        if is_target {
            let status = match deal.is_won {
                Some(1) => Some(TerminalStatus::Won),
                Some(0) => Some(TerminalStatus::Lost),
                _ => None,
            };
            target_deal = Some(TargetDealSummary {
                id: Some(deal.id),
                status_label: label,
                is_terminal: terminal,
                terminal_status: status,
            });
            if let Some(status) = status {
                let at = deal.updated_at.and_then(|at| {
                    parse_followup_timeline_at(&at.format("%Y-%m-%d %H:%M:%S").to_string())
                });
                terminal_deal = Some(FollowupTerminalDeal { status, at });
            }
        } else {
            other_deals.push(OtherDealSummary {
                status_label: label,
                is_terminal: terminal,
            });
        }
    }

    // Test databases and partial schemas skip the deal history.
    let (crm_hold, crm_note_hold) =
        match deal_history_lines(pool, customer_id, company_id, &mut lines, clock).await {
            Ok(holds) => holds,
            Err(err) => {
                lambda_http::tracing::warn!(
                    ?err,
                    customer_id,
                    "follow-up deal history unavailable"
                );
                (None, None)
            }
        };
    Ok(CustomerProfile {
        profile: truncate_text(&lines.join("\n"), MAX_PROFILE_CHARS),
        crm_hold,
        crm_note_hold,
        terminal_deal,
        target_deal,
        other_deals,
    })
}

/// Deal notes and activities into the profile, and the holds they set.
async fn deal_history_lines(
    pool: &MySqlPool,
    customer_id: i32,
    company_id: i32,
    lines: &mut Vec<String>,
    now: Instant,
) -> Result<(Option<CrmHoldUntil>, Option<CrmHoldUntil>), sqlx::Error> {
    use crate::routes::customers::history::{activities, notes};
    let iso_z = |at: Option<chrono::DateTime<chrono::Utc>>| {
        at.map(|at| at.format("%Y-%m-%dT%H:%M:%SZ").to_string())
    };
    let activities = activities(pool, company_id, customer_id).await?;
    let notes = notes(pool, company_id, customer_id).await?;
    let activity_likes: Vec<ActivityLike> = activities
        .iter()
        .map(|a| ActivityLike {
            name: Some(a.name.clone()),
            deadline: a.deadline.clone(),
            is_completed: a.is_completed != 0,
        })
        .collect();
    let note_likes: Vec<NoteLike> = notes
        .iter()
        .map(|n| NoteLike {
            content: Some(n.content.clone()),
            created_at: iso_z(n.created_at),
        })
        .collect();
    let crm_hold = resolve_crm_hold_until(&activity_likes, &note_likes, now);
    let crm_note_hold = crm_hold_from_notes(&note_likes, now);
    for note in notes.iter().take(40) {
        let when = format_when(&iso_z(note.created_at).unwrap_or_default());
        push_line(lines, &format!("Deal note ({when})"), Some(&note.content));
    }
    for activity in activities.iter().take(40) {
        let mut timing: Vec<String> = Vec::new();
        if let Some(created) = iso_z(activity.created_at) {
            timing.push(format!("created {}", format_when(&created)));
        }
        if let Some(deadline) = activity
            .deadline
            .as_deref()
            .filter(|_| activity.is_completed == 0)
        {
            if let Some(deadline_at) = parse_followup_timeline_at(deadline) {
                #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
                let overdue_days =
                    ((now - deadline_at).num_milliseconds() as f64 / 86_400_000.0).floor() as i64;
                if overdue_days > 0 {
                    timing.push(format!("deadline {overdue_days}d overdue"));
                } else if deadline_at > now {
                    timing.push(format!(
                        "deadline {} — do not contact the customer before this date",
                        format_when(deadline)
                    ));
                } else {
                    timing.push(format!("deadline {}", format_when(deadline)));
                }
            }
        }
        let label = re!(r"\s+", "g")
            .replace_all(&activity.name, " ")
            .trim()
            .to_string();
        lines.push(format!(
            "Activity: {label} · stage {} · {}{}",
            activity.deal_list_name.as_deref().unwrap_or("unknown"),
            if activity.is_completed != 0 {
                "completed"
            } else {
                "open"
            },
            if timing.is_empty() {
                String::new()
            } else {
                format!(" · {}", timing.join(" · "))
            }
        ));
    }
    Ok((crm_hold, crm_note_hold))
}

#[derive(sqlx::FromRow)]
struct SmsRow {
    text: Option<String>,
    direction: String,
    created_date: NaiveDateTime,
    sender: Option<String>,
}

/// Both providers' texts with these phones (a company that switched keeps
/// its earlier texts in the other table), the newest 1000, oldest first.
async fn load_sms_timeline(
    pool: &MySqlPool,
    company_id: i32,
    phones: &[String],
) -> Result<Vec<FollowupTimelineMessage>, sqlx::Error> {
    let mut digits: Vec<String> = Vec::new();
    for phone in phones {
        let all: String = phone.chars().filter(char::is_ascii_digit).collect();
        if all.len() < 10 {
            continue;
        }
        for value in [all.clone(), all[all.len() - 10..].to_string()] {
            if !digits.contains(&value) {
                digits.push(value);
            }
        }
    }
    if digits.is_empty() {
        return Ok(Vec::new());
    }
    let marks = vec!["?"; digits.len()].join(", ");
    let mut rows: Vec<SmsRow> = Vec::new();
    for table in ["cloudtalk_sms", "ringcentral_sms", "telnyx_sms"] {
        let normalized = |column: &str| {
            format!(
                "RIGHT(REPLACE(REPLACE(REPLACE(REPLACE(CAST({column} AS CHAR), ' ', ''), '-', ''), '(', ''), ')', ''), 10)"
            )
        };
        let sql = format!(
            "SELECT text, CAST(direction AS CHAR) AS direction, CAST(created_date AS DATETIME) AS created_date,
                    CAST(sender AS CHAR) AS sender
               FROM {table}
              WHERE company_id = ? AND status IN ('received', 'sent')
                AND ({} IN ({marks}) OR {} IN ({marks}))
              ORDER BY created_date DESC, id DESC LIMIT ?",
            normalized("sender"),
            normalized("recipient"),
        );
        let mut query = sqlx::query_as::<_, SmsRow>(sqlx::AssertSqlSafe(sql)).bind(company_id);
        for _ in 0..2 {
            for digit in &digits {
                query = query.bind(digit);
            }
        }
        rows.extend(
            query
                .bind(i64::try_from(MAX_SMS).unwrap_or(1000))
                .fetch_all(pool)
                .await?,
        );
    }
    rows.sort_by_key(|row| row.created_date);
    let skip = rows.len().saturating_sub(MAX_SMS);
    Ok(rows
        .into_iter()
        .skip(skip)
        .map(|row| {
            let direction = if row.direction == "inbound" {
                Direction::Inbound
            } else {
                Direction::Outbound
            };
            FollowupTimelineMessage {
                at: format_db_time(row.created_date),
                channel: FollowupChannel::Sms,
                direction,
                text: truncate_text(row.text.as_deref().unwrap_or(""), MAX_MSG_CHARS),
                from_email: None,
                from_phone: if direction == Direction::Inbound {
                    row.sender
                } else {
                    None
                },
            }
        })
        .collect())
}

async fn customer_email_addresses(
    pool: &MySqlPool,
    company_id: i32,
    customer_id: i32,
) -> Result<Vec<String>, sqlx::Error> {
    // Only the caller's company's customer (the CRM did not check).
    sqlx::query_scalar(
        "SELECT ce.email FROM customers_emails ce JOIN customers c ON c.id = ce.customer_id
          WHERE ce.customer_id = ? AND c.company_id = ?
          ORDER BY ce.id = c.email_id DESC, ce.created_at ASC, ce.id ASC",
    )
    .bind(customer_id)
    .bind(company_id)
    .fetch_all(pool)
    .await
}

async fn load_email_timeline(
    pool: &MySqlPool,
    company_id: i32,
    customer_id: Option<i32>,
    customer_addresses: &[String],
    extra: &[String],
) -> Result<Vec<FollowupTimelineMessage>, sqlx::Error> {
    use crate::routes::emails::deal_history::{address_emails, history_sql, merge};
    let Some(customer_id) = customer_id else {
        return Ok(Vec::new());
    };
    let mut addresses: Vec<String> = Vec::new();
    for address in customer_addresses.iter().chain(extra) {
        let normalized = trim(address).to_lowercase();
        if !normalized.is_empty() && !addresses.contains(&normalized) {
            addresses.push(normalized);
        }
    }
    let by_address = address_emails(pool, company_id, &addresses).await?;
    let by_participant = history_sql(
        "e.thread_id IN (SELECT DISTINCT e2.thread_id FROM emails e2
           JOIN email_participants ep ON ep.email_id = e2.id
           WHERE ep.customer_id = ? AND e2.thread_id IS NOT NULL AND e2.deleted_at IS NULL)",
        vec![company_id.into(), customer_id.into()],
    )
    .query_as()
    .fetch_all(pool)
    .await?;
    let mut history = merge(by_address, by_participant);
    history.truncate(MAX_EMAILS);
    history.reverse();
    Ok(history
        .into_iter()
        .map(|email| FollowupTimelineMessage {
            at: email
                .sent_at
                .map_or_else(String::new, |at| at.format("%Y-%m-%d %H:%M").to_string()),
            channel: FollowupChannel::Email,
            direction: if email.sender_user_id.is_some() {
                Direction::Outbound
            } else {
                Direction::Inbound
            },
            text: truncate_text(
                &format!(
                    "Subject: {}\n{}",
                    email
                        .subject
                        .as_deref()
                        .filter(|s| !s.is_empty())
                        .unwrap_or("No subject"),
                    strip_html_tags(&email.body)
                ),
                MAX_MSG_CHARS,
            ),
            from_email: email.sender_email,
            from_phone: None,
        })
        .collect())
}

/// The rep's own templates, then the company's (8 of each kind).
async fn load_templates(
    pool: &MySqlPool,
    company_id: i32,
    user_id: i32,
) -> Result<Vec<FollowupTemplateExample>, sqlx::Error> {
    let mut templates = Vec::new();
    for personal in [true, false] {
        let scope = if personal {
            TemplateScope::Personal
        } else {
            TemplateScope::Company
        };
        let owner = if personal {
            "user_id = ?"
        } else {
            "user_id IS NULL"
        };
        let sms_sql = format!(
            "SELECT name, body FROM cloudtalk_sms_templates
              WHERE company_id = ? AND deleted_at IS NULL AND {owner}
              ORDER BY position ASC, created_at DESC LIMIT ?"
        );
        let mut sms =
            sqlx::query_as::<_, (String, String)>(sqlx::AssertSqlSafe(sms_sql)).bind(company_id);
        if personal {
            sms = sms.bind(user_id);
        }
        let sms = sms.bind(MAX_TEMPLATES_PER_BUCKET).fetch_all(pool).await?;
        let email_sql = format!(
            "SELECT template_name, template_subject, template_body FROM email_templates
              WHERE company_id = ? AND deleted_at IS NULL AND {owner}
              ORDER BY created_at DESC LIMIT ?"
        );
        let mut email = sqlx::query_as::<_, (String, Option<String>, Option<String>)>(
            sqlx::AssertSqlSafe(email_sql),
        )
        .bind(company_id);
        if personal {
            email = email.bind(user_id);
        }
        let email = email.bind(MAX_TEMPLATES_PER_BUCKET).fetch_all(pool).await?;
        let sms: Vec<FollowupTemplateExample> = sms
            .into_iter()
            .map(|(name, body)| FollowupTemplateExample {
                name,
                subject: None,
                body: truncate_text(&body, MAX_TEMPLATE_BODY_CHARS),
                channel: Some(FollowupChannel::Sms),
                scope: Some(scope),
            })
            .collect();
        let email: Vec<FollowupTemplateExample> = email
            .into_iter()
            .map(|(name, subject, body)| FollowupTemplateExample {
                name,
                subject: Some(subject.unwrap_or_default()),
                body: truncate_text(
                    &strip_html_tags(body.as_deref().unwrap_or("")),
                    MAX_TEMPLATE_BODY_CHARS,
                ),
                channel: Some(FollowupChannel::Email),
                scope: Some(scope),
            })
            .collect();
        templates.extend(sms);
        templates.extend(email);
    }
    // The CRM's order: personal SMS, personal email, company SMS, company email.
    Ok(templates)
}

async fn load_company_instructions(
    pool: &MySqlPool,
    company_id: i32,
) -> Result<String, sqlx::Error> {
    let rows: Vec<(Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT title, rich_text FROM instructions WHERE company_id = ? AND (title IS NULL OR title <> ?) ORDER BY id",
    )
    .bind(company_id)
    .bind(super::instruction::AI_FOLLOWUP_ADMIN_INSTRUCTION_TITLE)
    .fetch_all(pool)
    .await?;
    if rows.is_empty() {
        return Ok(String::new());
    }
    let text = rows
        .iter()
        .map(|(title, body)| {
            let title = title
                .as_deref()
                .map(trim)
                .filter(|t| !t.is_empty())
                .unwrap_or("Untitled");
            format!(
                "{title}\n{}",
                trim(&strip_html_tags(body.as_deref().unwrap_or("")))
            )
        })
        .collect::<Vec<_>>()
        .join("\n\n");
    Ok(truncate_text(&text, MAX_INSTRUCTIONS_CHARS))
}

/// The company's customer page (`/customer/<id>/stones`).
pub fn customer_inventory_page_url(company_id: i32, domain: Option<&str>) -> String {
    format!("{}/stones", customer_pages_url(company_id, domain))
}

/// The company's online quote, where customers send their measurements
/// (`/customer/<id>/quote`).
pub fn customer_quote_page_url(company_id: i32, domain: Option<&str>) -> String {
    format!("{}/quote", customer_pages_url(company_id, domain))
}

/// `https://<subdomain>.granite-manager.com/customer/<id>`.
fn customer_pages_url(company_id: i32, domain: Option<&str>) -> String {
    let domain = trim(domain.unwrap_or(""));
    let without_scheme = re!(r"^https?:\/\/", "i").replace(domain, "");
    let slug = without_scheme
        .split('.')
        .next()
        .unwrap_or("")
        .to_lowercase();
    if slug.is_empty() {
        format!("https://granite-manager.com/customer/{company_id}")
    } else {
        format!("https://{slug}.granite-manager.com/customer/{company_id}")
    }
}

async fn load_customer_page_line(pool: &MySqlPool, company_id: i32) -> Result<String, sqlx::Error> {
    let domain: Option<Option<String>> =
        sqlx::query_scalar("SELECT domain FROM company WHERE id = ?")
            .bind(company_id)
            .fetch_optional(pool)
            .await?;
    let url = customer_inventory_page_url(company_id, domain.flatten().as_deref());
    Ok(format!(
        "Customer page URL for this company (id {company_id} — keep /customer/{company_id}/ exactly): {url}"
    ))
}

/// The rep's learned style, when stored.
pub async fn load_style_profile(
    pool: &MySqlPool,
    user_id: i32,
    company_id: i32,
) -> Result<Option<FollowupStyleProfile>, sqlx::Error> {
    let raw: Option<Option<String>> = sqlx::query_scalar(
        "SELECT CAST(preferences_json AS CHAR) FROM ai_followup_style_preferences WHERE user_id = ? AND company_id = ? LIMIT 1",
    )
    .bind(user_id)
    .bind(company_id)
    .fetch_optional(pool)
    .await?;
    Ok(raw
        .flatten()
        .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
        .and_then(|v| parse_followup_style_profile(&v)))
}

async fn customer_phones(
    pool: &MySqlPool,
    company_id: i32,
    customer_id: Option<i32>,
    fallback: Option<&str>,
) -> Result<Vec<String>, sqlx::Error> {
    let mut phones: Vec<String> = fallback.map(str::to_string).into_iter().collect();
    let Some(customer_id) = customer_id else {
        return Ok(phones);
    };
    let row: Option<(Option<String>, Option<String>)> =
        sqlx::query_as(
            "SELECT phone, phone_2 FROM customers WHERE id = ? AND company_id = ? AND deleted_at IS NULL",
        )
        .bind(customer_id)
        .bind(company_id)
            .fetch_optional(pool)
            .await?;
    if let Some((phone, phone_2)) = row {
        phones.extend(phone.filter(|p| !p.is_empty()));
        phones.extend(phone_2.filter(|p| !p.is_empty()));
    }
    Ok(phones)
}

/// The customer of the request: given, or found by phone or email.
pub async fn resolve_customer_id(
    pool: &MySqlPool,
    request: &FollowupRequest,
) -> Result<Option<i32>, sqlx::Error> {
    if let Some(customer_id) = request.customer_id {
        // Ids come from request bodies: only a customer of the company.
        return sqlx::query_scalar("SELECT id FROM customers WHERE id = ? AND company_id = ?")
            .bind(customer_id)
            .bind(request.company_id)
            .fetch_optional(pool)
            .await;
    }
    let Some(phone) = request.phone_digits.as_deref() else {
        return match request.customer_email.as_deref() {
            Some(email) => customer_by_email(pool, request.company_id, email).await,
            None => Ok(None),
        };
    };
    let customer = crate::routes::sms::thread::customer_by_phone(
        pool,
        crate::routes::sms::provider::SmsProvider::Cloudtalk,
        request.company_id,
        phone,
    )
    .await?;
    Ok(customer.map(|c| c.id))
}

/// The customer with this address, or else the newest customer in an email
/// thread with it (a spouse writing from their own address).
async fn customer_by_email(
    pool: &MySqlPool,
    company_id: i32,
    email: &str,
) -> Result<Option<i32>, sqlx::Error> {
    let email = trim(email).to_lowercase();
    if email.is_empty() {
        return Ok(None);
    }
    let own: Option<i32> = sqlx::query_scalar(
        "SELECT c.id FROM customers_emails ce JOIN customers c ON c.id = ce.customer_id
          WHERE ce.email = ? AND c.company_id = ? AND c.deleted_at IS NULL
          ORDER BY c.id DESC LIMIT 1",
    )
    .bind(&email)
    .bind(company_id)
    .fetch_optional(pool)
    .await?;
    if own.is_some() {
        return Ok(own);
    }
    // Employees' addresses can be linked to customers too: skip them.
    sqlx::query_scalar(
        "SELECT c.id FROM email_participants ep
           JOIN emails e ON e.id = ep.email_id AND e.company_id = ? AND e.deleted_at IS NULL
           JOIN emails t ON t.company_id = e.company_id AND t.thread_id = e.thread_id AND t.deleted_at IS NULL
           JOIN email_participants other ON other.email_id = t.id AND other.customer_id IS NOT NULL
                                        AND other.user_id IS NULL
           JOIN customers c ON c.id = other.customer_id AND c.company_id = e.company_id
                           AND c.deleted_at IS NULL
          WHERE ep.email = ? AND e.thread_id IS NOT NULL
          ORDER BY t.sent_at DESC, t.id DESC LIMIT 1",
    )
    .bind(company_id)
    .bind(&email)
    .fetch_optional(pool)
    .await
}

/// Keeps the newest messages within 80,000 characters.
fn trim_timeline(messages: Vec<FollowupTimelineMessage>) -> Vec<FollowupTimelineMessage> {
    let mut total = 0;
    let mut kept: Vec<FollowupTimelineMessage> = Vec::new();
    for message in messages.into_iter().rev() {
        let next = total + js_len(&message.text);
        if next > MAX_TOTAL_CONTEXT_CHARS && !kept.is_empty() {
            break;
        }
        kept.push(message);
        total = next;
    }
    kept.reverse();
    kept
}

/// The contact pressure, its validation, and whether to write.
#[allow(clippy::too_many_arguments)]
fn decide(
    request: &FollowupRequest,
    timeline: &[FollowupTimelineMessage],
    profile: &str,
    profile_result: &CustomerProfile,
    identity_emails: &[String],
    extra_emails: &[String],
    phones: Vec<String>,
    clock: Instant,
    mark: &mut impl FnMut(&'static str),
) -> (
    FollowupContactPressure,
    super::pressure::repair::FollowupValidationResult,
    FollowupGenerationDecision,
) {
    let mut emails_known: Vec<String> = Vec::new();
    for email in identity_emails.iter().chain(extra_emails) {
        if !emails_known.contains(email) {
            emails_known.push(email.clone());
        }
    }
    let raw_pressure = build_followup_contact_pressure(&PressureInputs {
        timeline: timeline
            .iter()
            .map(FollowupTimelineMessage::to_timeline)
            .collect(),
        now: Some(clock),
        crm_hold_until: profile_result.crm_hold.as_ref().map(|h| h.until),
        crm_note_hold_until: profile_result.crm_note_hold.as_ref().map(|h| h.until),
        explicit_do_not_contact_until: None,
        terminal_deal: profile_result.terminal_deal,
        profile: profile.to_string(),
        customer_emails: Some(emails_known),
        customer_phones: Some(phones),
        target_deal: profile_result.target_deal.clone(),
        other_deals: profile_result.other_deals.clone(),
        explicit_reenroll: request.explicit_reenroll,
    });
    mark("fact_extraction");
    let validation = prepare_followup_validation(&raw_pressure);
    mark("validation");
    let pressure = validation.pressure.clone();
    let generation_decision = evaluate_followup_generation_decision(&GenerationDecisionInputs {
        sequence_status: Some(pressure.followup_sequence_status),
        latest_intent: pressure.latest_meaningful_customer_intent.as_ref(),
        target_deal_terminal: match pressure.deal_state {
            DealState::Won => Some(TargetDealTerminal::Won),
            DealState::Lost => Some(TargetDealTerminal::Lost),
            _ => None,
        },
        latest_customer_state: Some(pressure.latest_customer_state),
        explicit_reenroll: request.explicit_reenroll,
        customer_reply_handled: pressure.customer_reply_handled,
        validation_errors: validation.hard_block_errors.clone(),
        contact_resume_mode: Some(pressure.contact_resume_mode),
        automation_contact_status: Some(pressure.automation_contact_status),
        sms_available: Some(pressure.sms_available),
        email_available: Some(pressure.email_available),
        customer_waiting_for_human_reply: pressure.customer_waiting_for_human_reply,
        rep_owned_next_step: pressure.rep_owned_next_step,
        sms_permission: Some(pressure.sms_permission),
    });
    (pressure, validation, generation_decision)
}

/// Loads everything a follow-up is written from and decides whether to write.
/// `clock` is the wall clock (holds, overdue tasks, days since contact).
pub async fn gather_followup_context(
    pool: &MySqlPool,
    request: &FollowupRequest,
    clock: Instant,
) -> Result<GatheredContext, sqlx::Error> {
    gather_timed(pool, request, clock, &mut None).await
}

/// [`gather_followup_context`], marking its phases on `timer`.
#[allow(clippy::too_many_lines)]
pub async fn gather_timed(
    pool: &MySqlPool,
    request: &FollowupRequest,
    clock: Instant,
    timer: &mut Option<&mut super::pipeline::PipelineTimer>,
) -> Result<GatheredContext, sqlx::Error> {
    let mut mark = |phase: &'static str| {
        if let Some(timer) = timer.as_deref_mut() {
            timer.mark(phase);
        }
    };
    let customer_id = resolve_customer_id(pool, request).await?;
    mark("resolve_customer");
    let phones = customer_phones(
        pool,
        request.company_id,
        customer_id,
        request.phone_digits.as_deref(),
    )
    .await?;
    let extra_emails: Vec<String> = request.customer_email.iter().cloned().collect();
    let customer_addresses = match customer_id {
        Some(id) => customer_email_addresses(pool, request.company_id, id).await?,
        None => Vec::new(),
    };
    let identity_emails = if customer_id.is_some() {
        customer_addresses.clone()
    } else {
        extra_emails.clone()
    };
    let profile_result = match customer_id {
        Some(id) => {
            load_customer_profile(
                pool,
                id,
                request.company_id,
                request.preferred_deal_id,
                clock,
            )
            .await?
        }
        None => CustomerProfile {
            profile: request
                .phone_digits
                .as_ref()
                .map(|p| format!("Phone: {p}"))
                .or_else(|| {
                    request
                        .customer_email
                        .as_ref()
                        .map(|e| format!("Email: {e}"))
                })
                .unwrap_or_default(),
            crm_hold: None,
            crm_note_hold: None,
            terminal_deal: None,
            target_deal: None,
            other_deals: Vec::new(),
        },
    };
    let sms = load_sms_timeline(pool, request.company_id, &phones).await?;
    let emails = load_email_timeline(
        pool,
        request.company_id,
        customer_id,
        &customer_addresses,
        &extra_emails,
    )
    .await?;
    let templates = load_templates(pool, request.company_id, request.user_id).await?;
    let company_instructions = load_company_instructions(pool, request.company_id).await?;
    let admin_instruction =
        crate::routes::followups::instruction::load(pool, request.company_id).await?;
    let customer_page_line = load_customer_page_line(pool, request.company_id).await?;
    let style_profile = load_style_profile(pool, request.user_id, request.company_id).await?;
    // Best effort: without it the draft only loses the style examples.
    let rep_voice = super::rep_voice::load_rep_voice(pool, request.company_id, request.user_id)
        .await
        .unwrap_or_else(|err| {
            lambda_http::tracing::warn!(?err, "follow-up rep messages unavailable");
            super::rep_voice::RepVoice::default()
        });
    let (sms_loaded, emails_loaded) = (sms.len(), emails.len());
    mark("db_load");

    let mut merged: Vec<FollowupTimelineMessage> = sms.into_iter().chain(emails).collect();
    merged.sort_by(|a, b| a.at.cmp(&b.at));
    let timeline = trim_timeline(merged);
    let profile = [profile_result.profile.as_str(), customer_page_line.as_str()]
        .into_iter()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    mark("conversation_normalization");
    let (pressure, validation, generation_decision) = decide(
        request,
        &timeline,
        &profile,
        &profile_result,
        &identity_emails,
        &extra_emails,
        phones,
        clock,
        &mut mark,
    );
    let engine_timeline: Vec<_> = timeline
        .iter()
        .map(FollowupTimelineMessage::to_timeline)
        .collect();
    Ok(GatheredContext {
        customer_id,
        situation: classify_followup_situation(&engine_timeline, pressure.has_relationship_history),
        profile,
        timeline,
        templates,
        company_instructions,
        admin_instruction,
        crm_hold: profile_result.crm_hold,
        crm_note_hold: profile_result.crm_note_hold,
        contact_pressure: pressure,
        generation_decision,
        style_profile,
        rep_voice,
        validation_status: validation.validation_status,
        validation_repairs: validation.validation_repairs,
        sms_loaded,
        emails_loaded,
    })
}
