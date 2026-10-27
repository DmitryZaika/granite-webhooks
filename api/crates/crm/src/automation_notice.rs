//! The email the company's sales reps get when an admin changes an automation: a
//! drip (a company email or SMS template that sends itself when a deal
//! enters a list) or a company follow-up flow. It says what changed, what
//! customers will now receive automatically, and under which conditions.
//!
//! Handlers take a [`Snapshot`] before and after their write and call
//! [`notify`]; nothing is sent when no automation is involved or nothing a
//! customer would see changed. A failed send is logged, never an error of
//! the save.

use crate::auth::session::SessionUser;
use crate::routes::customers::service::SALES_REP_POSITION;
use crate::routes::emails::signature_mail::{Branding, card_html};
use crate::state::AppState;
use crate::webhooks_client::OutgoingEmail;
use common::utils::email_body::{escape_html, html_to_plain_text, is_html_markup};
use sqlx::MySqlPool;
use std::fmt::Write as _;

const FONT: &str = "font-family:Inter,'Helvetica Neue',Arial,sans-serif;";
/// Longest message preview, in characters.
const PREVIEW_CHARS: usize = 700;

/// How a message reaches the customer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Channel {
    Email,
    Sms,
}

/// When a drip template sends itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Drip {
    /// "Pipeline › List" of the deal list that starts it.
    pub list: String,
    /// Minutes after the deal enters the list.
    pub minutes: i32,
    /// Sales reps whose deals get it, by name; empty: every rep's.
    pub reps: Vec<Rep>,
}

/// A sales rep a drip is limited to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rep {
    pub id: i32,
    pub name: String,
}

/// "every sales rep", or the reps' names.
fn whose(reps: &[Rep]) -> String {
    if reps.is_empty() {
        return "every sales rep".to_string();
    }
    let names: Vec<String> = reps.iter().map(|rep| escape_html(&rep.name)).collect();
    format!("<b>{}</b>", names.join(", "))
}

/// A company template as customers would get it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Template {
    pub channel: Channel,
    pub name: String,
    /// Email templates only.
    pub subject: Option<String>,
    /// HTML (email) or text (SMS), with `{{variables}}`.
    pub body: String,
    pub drip: Option<Drip>,
}

/// One step of a flow.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Step {
    pub channel: Channel,
    pub subject: Option<String>,
    pub body: String,
    /// Minutes after the previous step (the first: after the start).
    pub delay_minutes: i32,
}

/// A company follow-up flow.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Flow {
    pub name: String,
    pub steps: Vec<Step>,
}

/// An automation's state on one side of a save; `None`: it did not exist.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Snapshot {
    Template(Option<Template>),
    Flow(Option<Flow>),
}

/// The email to send, when the change is worth one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notice {
    pub subject: String,
    pub html: String,
}

enum Kind {
    Added,
    Updated,
    Removed,
}

impl Kind {
    const fn of<T>(before: Option<&T>, after: Option<&T>) -> Self {
        match (before, after) {
            (None, _) => Self::Added,
            (_, None) => Self::Removed,
            _ => Self::Updated,
        }
    }
}

/// The notice for a change from `before` to `after`, made by `editor`;
/// `None` when no automation is involved or nothing changed.
pub fn compose(
    before: &Snapshot,
    after: &Snapshot,
    editor: &str,
    branding: &Branding,
) -> Option<Notice> {
    match (before, after) {
        (Snapshot::Template(before), Snapshot::Template(after)) => {
            template_notice(before.as_ref(), after.as_ref(), editor, branding)
        }
        (Snapshot::Flow(before), Snapshot::Flow(after)) => {
            flow_notice(before.as_ref(), after.as_ref(), editor, branding)
        }
        _ => None,
    }
}

fn template_notice(
    before: Option<&Template>,
    after: Option<&Template>,
    editor: &str,
    branding: &Branding,
) -> Option<Notice> {
    let was_drip = before.and_then(|t| t.drip.as_ref());
    let is_drip = after.and_then(|t| t.drip.as_ref());
    if (was_drip.is_none() && is_drip.is_none()) || before == after {
        return None;
    }
    let current = after.or(before)?;
    let channel = current.channel;
    let noun = channel_noun(channel);
    let name = &current.name;
    let (subject, headline, intro) = match (was_drip, is_drip) {
        (None, Some(_)) => (
            format!("New automatic {noun}: {name}"),
            format!("A new automatic {noun} is on"),
            format!(
                "{editor} set the {noun} template <b>{}</b> to send itself. From now on customers get it automatically:",
                escape_html(name)
            ),
        ),
        (Some(_), None) => (
            format!("Automatic {noun} turned off: {name}"),
            format!("An automatic {noun} is off"),
            format!(
                "{editor} {} the {noun} template <b>{}</b>. Customers no longer get it automatically, and the ones still waiting for it won't receive it. This is what used to go out:",
                if after.is_none() {
                    "deleted"
                } else {
                    "turned off automatic sending for"
                },
                escape_html(name)
            ),
        ),
        _ => (
            format!("Automatic {noun} updated: {name}"),
            format!("An automatic {noun} was updated"),
            format!(
                "{editor} changed the automatic {noun} <b>{}</b>. This is what customers get from now on:",
                escape_html(name)
            ),
        ),
    };
    let shown = if is_drip.is_some() { after? } else { before? };
    let drip = shown.drip.as_ref()?;
    let changes = match (before, after) {
        (Some(before), Some(after)) if was_drip.is_some() && is_drip.is_some() => {
            template_changes(before, after)
        }
        _ => Vec::new(),
    };
    let message = message_card(
        channel,
        &format!(
            "{} after a deal enters <b>{}</b>{}",
            duration(drip.minutes, true),
            escape_html(&drip.list),
            if drip.reps.is_empty() {
                String::new()
            } else {
                format!(", for deals of {}", whose(&drip.reps))
            }
        ),
        shown.subject.as_deref(),
        &shown.body,
    );
    let html = layout(
        branding,
        &headline,
        &intro,
        &changes,
        &section("What customers receive", &message),
        &drip_conditions(channel),
    );
    Some(Notice {
        subject: subject_line(&subject),
        html,
    })
}

fn template_changes(before: &Template, after: &Template) -> Vec<String> {
    let mut changes = Vec::new();
    if before.name != after.name {
        changes.push(format!(
            "Renamed from <b>{}</b> to <b>{}</b>.",
            escape_html(&before.name),
            escape_html(&after.name)
        ));
    }
    if let (Some(was), Some(now)) = (&before.drip, &after.drip) {
        if was.list != now.list {
            changes.push(format!(
                "Now starts when a deal enters <b>{}</b> (was <b>{}</b>).",
                escape_html(&now.list),
                escape_html(&was.list)
            ));
        }
        if was.minutes != now.minutes {
            changes.push(format!(
                "Now sends <b>{}</b> after that (was {}).",
                duration(now.minutes, false),
                duration(was.minutes, false)
            ));
        }
        if was.reps != now.reps {
            changes.push(format!(
                "Now for deals of {} (was {}).",
                whose(&now.reps),
                whose(&was.reps)
            ));
        }
    }
    if before.subject != after.subject {
        changes.push(format!(
            "New subject line: <b>{}</b>.",
            escape_html(after.subject.as_deref().unwrap_or_default())
        ));
    }
    if before.body != after.body {
        changes.push("The message text changed (see below).".to_string());
    }
    changes
}

fn flow_notice(
    before: Option<&Flow>,
    after: Option<&Flow>,
    editor: &str,
    branding: &Branding,
) -> Option<Notice> {
    if before == after {
        return None;
    }
    let shown = after.or(before)?;
    let name = &shown.name;
    let (subject, headline, intro) = match Kind::of(before, after) {
        Kind::Added => (
            format!("New follow-up flow: {name}"),
            "A new follow-up flow is ready".to_string(),
            format!(
                "{editor} added the company follow-up flow <b>{}</b>. Start it for a customer and it sends these messages for you:",
                escape_html(name)
            ),
        ),
        Kind::Removed => (
            format!("Follow-up flow deleted: {name}"),
            "A follow-up flow was deleted".to_string(),
            format!(
                "{editor} deleted the company follow-up flow <b>{}</b>. Customers who were in it stop receiving it. These were its messages:",
                escape_html(name)
            ),
        ),
        Kind::Updated => (
            format!("Follow-up flow updated: {name}"),
            "A follow-up flow was updated".to_string(),
            format!(
                "{editor} changed the company follow-up flow <b>{}</b>. Customers already in it continue with the new steps and timing:",
                escape_html(name)
            ),
        ),
    };
    let changes = match (before, after) {
        (Some(before), Some(after)) => flow_changes(before, after),
        _ => Vec::new(),
    };
    let mut steps = String::new();
    for (index, step) in shown.steps.iter().enumerate() {
        let when = if index == 0 {
            if step.delay_minutes == 0 {
                "Right when the flow starts".to_string()
            } else {
                format!(
                    "{} after the flow starts",
                    duration(step.delay_minutes, true)
                )
            }
        } else if step.delay_minutes == 0 {
            "Right after the previous step".to_string()
        } else {
            format!(
                "{} after the previous step",
                duration(step.delay_minutes, true)
            )
        };
        steps.push_str(&message_card(
            step.channel,
            &format!("<b>Step {}</b> · {when}", index + 1),
            step.subject.as_deref(),
            &step.body,
        ));
    }
    let html = layout(
        branding,
        &headline,
        &intro,
        &changes,
        &section("The messages, in order", &steps),
        &flow_conditions(),
    );
    Some(Notice {
        subject: subject_line(&subject),
        html,
    })
}

fn flow_changes(before: &Flow, after: &Flow) -> Vec<String> {
    let mut changes = Vec::new();
    if before.name != after.name {
        changes.push(format!(
            "Renamed from <b>{}</b> to <b>{}</b>.",
            escape_html(&before.name),
            escape_html(&after.name)
        ));
    }
    let shared = before.steps.len().min(after.steps.len());
    for index in 0..shared {
        let (was, now) = (&before.steps[index], &after.steps[index]);
        let number = index + 1;
        if was.channel != now.channel {
            changes.push(format!(
                "Step {number} is now {} (was {}).",
                channel_label(now.channel),
                channel_label(was.channel)
            ));
        }
        if was.delay_minutes != now.delay_minutes {
            changes.push(format!(
                "Step {number} now waits <b>{}</b> (was {}).",
                duration(now.delay_minutes, false),
                duration(was.delay_minutes, false)
            ));
        }
        if was.body != now.body || was.subject != now.subject {
            changes.push(format!("Step {number} has new text."));
        }
    }
    match after.steps.len().cmp(&before.steps.len()) {
        std::cmp::Ordering::Greater => {
            let added = after.steps.len() - before.steps.len();
            changes.push(if added == 1 {
                format!("Step {} was added.", after.steps.len())
            } else {
                format!("{added} steps were added at the end.")
            });
        }
        std::cmp::Ordering::Less => {
            let removed = before.steps.len() - after.steps.len();
            changes.push(if removed == 1 {
                format!("The last step (step {}) was removed.", before.steps.len())
            } else {
                format!("The last {removed} steps were removed.")
            });
        }
        std::cmp::Ordering::Equal => {}
    }
    changes
}

const fn channel_noun(channel: Channel) -> &'static str {
    match channel {
        Channel::Email => "email",
        Channel::Sms => "text",
    }
}

const fn channel_label(channel: Channel) -> &'static str {
    match channel {
        Channel::Email => "an email",
        Channel::Sms => "a text",
    }
}

/// `2 days`, `1 day 4 hours`, `30 minutes`; zero is "right away"
/// (`Right away` when `capital`).
fn duration(minutes: i32, capital: bool) -> String {
    if minutes <= 0 {
        return if capital { "Right away" } else { "right away" }.to_string();
    }
    let unit =
        |value: i32, name: &str| format!("{value} {name}{}", if value == 1 { "" } else { "s" });
    let (days, hours, mins) = (minutes / 1440, minutes % 1440 / 60, minutes % 60);
    let parts: Vec<String> = [(days, "day"), (hours, "hour"), (mins, "minute")]
        .into_iter()
        .filter(|(value, _)| *value > 0)
        .map(|(value, name)| unit(value, name))
        .collect();
    parts.join(" ")
}

fn subject_line(subject: &str) -> String {
    let subject: String = subject.chars().take(150).collect();
    format!("Automation change · {subject}")
}

/// The message as readable text, `{{variables}}` shown as labels.
fn preview_html(body: &str) -> String {
    let text = if is_html_markup(body) {
        html_to_plain_text(body)
    } else {
        body.to_string()
    };
    let text = text.trim();
    let mut clipped: String = text.chars().take(PREVIEW_CHARS).collect();
    if text.chars().count() > PREVIEW_CHARS {
        clipped.push('…');
    }
    let mut out = String::new();
    let mut rest = clipped.as_str();
    while let Some(start) = rest.find("{{") {
        let Some(end) = rest[start..].find("}}") else {
            break;
        };
        out.push_str(&escape_html(&rest[..start]));
        let key = rest[start + 2..start + end].trim();
        let _ = write!(
            out,
            "<span style=\"background:#fef3c7;color:#92400e;border-radius:4px;padding:1px 6px;font-size:13px;white-space:nowrap;\">{}</span>",
            escape_html(&variable_label(key))
        );
        rest = &rest[start + end + 2..];
    }
    out.push_str(&escape_html(rest));
    out.replace('\n', "<br>")
}

/// `customer.first_name` → `Customer first name`.
fn variable_label(key: &str) -> String {
    let words = key.replace(['.', '_'], " ");
    let mut chars = words.trim().chars();
    chars.next().map_or_else(String::new, |first| {
        first.to_uppercase().chain(chars).collect()
    })
}

/// One message: channel badge, when it goes out, subject and text.
fn message_card(channel: Channel, when: &str, subject: Option<&str>, body: &str) -> String {
    let (badge, color, background) = match channel {
        Channel::Email => ("✉️ EMAIL", "#1d4ed8", "#dbeafe"),
        Channel::Sms => ("💬 TEXT", "#047857", "#d1fae5"),
    };
    let subject_row = subject
        .map(str::trim)
        .filter(|subject| !subject.is_empty())
        .map_or_else(String::new, |subject| {
            format!(
                "<div style=\"{FONT}font-size:14px;color:#18181b;padding:10px 0 0;\"><span style=\"color:#71717a;\">Subject:</span> <b>{}</b></div>",
                escape_html(subject)
            )
        });
    format!(
        "<tr><td style=\"padding:0 0 14px;\">
  <table role=\"presentation\" width=\"100%\" cellpadding=\"0\" cellspacing=\"0\" style=\"border:1px solid #e4e4e7;border-left:4px solid {color};border-radius:8px;\">
    <tr><td style=\"padding:14px 16px;\">
      <span style=\"{FONT}display:inline-block;font-size:11px;font-weight:700;letter-spacing:.04em;color:{color};background:{background};border-radius:999px;padding:3px 10px;\">{badge}</span>
      <span style=\"{FONT}font-size:14px;color:#3f3f46;padding-left:8px;\">{when}</span>
      {subject_row}
      <div style=\"{FONT}font-size:14px;line-height:1.55;color:#3f3f46;background:#fafafa;border-radius:6px;padding:12px 14px;margin-top:10px;\">{}</div>
    </td></tr>
  </table>
</td></tr>",
        preview_html(body)
    )
}

fn section(title: &str, rows: &str) -> String {
    format!(
        "<tr><td style=\"{FONT}font-size:13px;font-weight:700;letter-spacing:.06em;text-transform:uppercase;color:#71717a;padding:22px 0 12px;\">{title}</td></tr>{rows}"
    )
}

fn bullets(items: &[String]) -> String {
    let items = items.iter().fold(String::new(), |mut out, item| {
        let _ = write!(
            out,
            "<tr><td valign=\"top\" style=\"{FONT}font-size:14px;color:#a16207;padding:3px 10px 3px 0;\">●</td><td style=\"{FONT}font-size:14px;line-height:1.5;color:#3f3f46;padding:3px 0;\">{item}</td></tr>"
        );
        out
    });
    format!(
        "<tr><td><table role=\"presentation\" cellpadding=\"0\" cellspacing=\"0\">{items}</table></td></tr>"
    )
}

const WINDOWS: &str = "Texts go out between 9:00 AM and 6:00 PM and emails between 8:30 AM and 7:00 PM (Eastern Time), never on New Year's Day, July 4, Thanksgiving, Christmas Eve or Christmas. A message due outside those hours waits for the next opening.";

fn drip_conditions(channel: Channel) -> Vec<String> {
    let mut items = vec![
        "Only for customers whose source is <b>Leads</b>, on a deal that has an owner. For a new deal, only when it is the customer's first deal.".to_string(),
        "Each customer gets it once. It goes out from the deal owner's own email address or text line.".to_string(),
        "It is called off if the deal moves to another list before the send time, or the customer stops being a lead.".to_string(),
        WINDOWS.to_string(),
    ];
    match channel {
        Channel::Sms => items.push(
            "The customer needs a phone number, and the deal owner a connected text line; otherwise the text is not sent.".to_string(),
        ),
        Channel::Email => items.push(
            "The customer needs an email address; otherwise the email is not sent.".to_string(),
        ),
    }
    items
}

fn flow_conditions() -> Vec<String> {
    vec![
        "It never starts on its own: a rep starts it for a customer from the customer or their text thread.".to_string(),
        "Each step goes out after the wait shown, counted from the previous step. It goes out from the rep's own email address or text line.".to_string(),
        "It stops as soon as the customer replies by text or email, or calls.".to_string(),
        WINDOWS.to_string(),
    ]
}

fn layout(
    branding: &Branding,
    headline: &str,
    intro: &str,
    changes: &[String],
    message: &str,
    conditions: &[String],
) -> String {
    let changes = if changes.is_empty() {
        String::new()
    } else {
        format!(
            "<tr><td style=\"padding-top:18px;\"><table role=\"presentation\" width=\"100%\" cellpadding=\"0\" cellspacing=\"0\" style=\"background:#fffbeb;border:1px solid #fde68a;border-radius:8px;\"><tr><td style=\"padding:14px 16px;\"><div style=\"{FONT}font-size:13px;font-weight:700;letter-spacing:.06em;text-transform:uppercase;color:#a16207;padding-bottom:6px;\">What changed</div><table role=\"presentation\" cellpadding=\"0\" cellspacing=\"0\">{}</table></td></tr></table></td></tr>",
            bullets(changes)
        )
    };
    card_html(
        branding,
        &format!(
            "<tr><td align=\"center\" style=\"{FONT}font-size:22px;font-weight:700;color:#18181b;padding-bottom:12px;\">{}</td></tr>
          <tr><td style=\"{FONT}font-size:15px;line-height:1.6;color:#3f3f46;\">{intro}</td></tr>
          {changes}
          {message}
          {}
          <tr><td style=\"{FONT}font-size:12px;line-height:1.5;color:#a1a1aa;padding-top:22px;\">You get this because you are a sales rep at {}: these messages go out from your email address and text line. Questions about it? Reply to this email to reach the admin who made the change.</td></tr>",
            escape_html(headline),
            section("When it is sent", &bullets(conditions)),
            escape_html(&branding.company_name),
        ),
    )
}

/// "Pipeline › List" of a deal list.
async fn list_label(pool: &MySqlPool, list_id: i32) -> Result<String, sqlx::Error> {
    let row: Option<(String, Option<String>)> = sqlx::query_as(
        "SELECT dl.name, gl.name FROM deals_list dl
           LEFT JOIN groups_list gl ON gl.id = dl.group_id
          WHERE dl.id = ?",
    )
    .bind(list_id)
    .fetch_optional(pool)
    .await?;
    Ok(match row {
        Some((list, Some(group))) => format!("{group} › {list}"),
        Some((list, None)) => list,
        None => format!("list #{list_id}"),
    })
}

/// A company email template as customers get it; `None` for a personal or
/// missing one.
pub async fn email_template(pool: &MySqlPool, company_id: i32, id: i32) -> Snapshot {
    Snapshot::Template(
        load_email_template(pool, company_id, id)
            .await
            .unwrap_or_else(|err| {
                lambda_http::tracing::warn!(
                    ?err,
                    id,
                    "automation notice: could not read the email template"
                );
                None
            }),
    )
}

async fn load_email_template(
    pool: &MySqlPool,
    company_id: i32,
    id: i32,
) -> Result<Option<Template>, sqlx::Error> {
    #[allow(clippy::type_complexity)]
    let row: Option<(
        Option<String>,
        Option<String>,
        Option<String>,
        Option<i32>,
        Option<i32>,
        Option<String>,
    )> = sqlx::query_as(
        "SELECT template_name, template_subject, template_body, lead_list_id, hour_delay,
                CAST(sales_rep_ids AS CHAR)
               FROM email_templates
              WHERE id = ? AND company_id = ? AND user_id IS NULL AND deleted_at IS NULL",
    )
    .bind(id)
    .bind(company_id)
    .fetch_optional(pool)
    .await?;
    let Some((name, subject, body, list, hours, reps)) = row else {
        return Ok(None);
    };
    Ok(Some(Template {
        channel: Channel::Email,
        name: name.unwrap_or_default(),
        subject,
        body: body.unwrap_or_default(),
        drip: drip(pool, list, hours.map(|hours| hours * 60), reps.as_deref()).await?,
    }))
}

/// A company SMS template as customers get it; `None` for a personal or
/// missing one.
pub async fn sms_template(pool: &MySqlPool, company_id: i32, id: i32) -> Snapshot {
    Snapshot::Template(
        load_sms_template(pool, company_id, id)
            .await
            .unwrap_or_else(|err| {
                lambda_http::tracing::warn!(
                    ?err,
                    id,
                    "automation notice: could not read the SMS template"
                );
                None
            }),
    )
}

async fn load_sms_template(
    pool: &MySqlPool,
    company_id: i32,
    id: i32,
) -> Result<Option<Template>, sqlx::Error> {
    #[allow(clippy::type_complexity)]
    let row: Option<(String, String, Option<i32>, Option<i32>, i32, Option<String>)> =
        sqlx::query_as(
            "SELECT name, body, lead_list_id, hour_delay, minute_delay, CAST(sales_rep_ids AS CHAR)
               FROM cloudtalk_sms_templates
              WHERE id = ? AND company_id = ? AND user_id IS NULL AND deleted_at IS NULL",
        )
        .bind(id)
        .bind(company_id)
        .fetch_optional(pool)
        .await?;
    let Some((name, body, list, hours, minutes, reps)) = row else {
        return Ok(None);
    };
    Ok(Some(Template {
        channel: Channel::Sms,
        name,
        subject: None,
        body,
        drip: drip(
            pool,
            list,
            hours.map(|hours| hours * 60 + minutes),
            reps.as_deref(),
        )
        .await?,
    }))
}

async fn drip(
    pool: &MySqlPool,
    list: Option<i32>,
    minutes: Option<i32>,
    reps: Option<&str>,
) -> Result<Option<Drip>, sqlx::Error> {
    match (list, minutes) {
        (Some(list), Some(minutes)) => Ok(Some(Drip {
            list: list_label(pool, list).await?,
            minutes,
            reps: rep_names(pool, reps).await?,
        })),
        _ => Ok(None),
    }
}

/// The reps of a stored `sales_rep_ids`, by name; empty: every rep.
async fn rep_names(pool: &MySqlPool, json: Option<&str>) -> Result<Vec<Rep>, sqlx::Error> {
    let ids: Vec<i32> = json
        .and_then(|raw| serde_json::from_str(raw).ok())
        .unwrap_or_default();
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let placeholders = vec!["?"; ids.len()].join(", ");
    let mut query = sqlx::query_as::<_, (i32, Option<String>)>(sqlx::AssertSqlSafe(format!(
        "SELECT id, name FROM users WHERE id IN ({placeholders}) ORDER BY name, id"
    )));
    for id in &ids {
        query = query.bind(id);
    }
    Ok(query
        .fetch_all(pool)
        .await?
        .into_iter()
        .map(|(id, name)| Rep {
            id,
            name: name.unwrap_or_default().trim().to_string(),
        })
        .collect())
}

/// Whose deals an automation reaches; `None`: no one (not a drip).
enum Reach {
    EveryRep,
    Reps(Vec<i32>),
}

fn reach(snapshot: &Snapshot) -> Option<Reach> {
    match snapshot {
        Snapshot::Template(template) => {
            let drip = template.as_ref()?.drip.as_ref()?;
            Some(if drip.reps.is_empty() {
                Reach::EveryRep
            } else {
                Reach::Reps(drip.reps.iter().map(|rep| rep.id).collect())
            })
        }
        Snapshot::Flow(_) => Some(Reach::EveryRep),
    }
}

/// Addresses of the reps the change touches: whose deals got the automation
/// before or get it now.
async fn recipients(
    pool: &MySqlPool,
    company_id: i32,
    before: &Snapshot,
    after: &Snapshot,
) -> Result<Vec<String>, sqlx::Error> {
    let mut ids: Vec<i32> = Vec::new();
    for reach in [reach(before), reach(after)].into_iter().flatten() {
        match reach {
            Reach::EveryRep => {
                return sqlx::query_scalar(
                    "SELECT DISTINCT TRIM(u.email) FROM users u
                       JOIN users_positions up
                         ON up.user_id = u.id AND up.company_id = u.company_id AND up.position_id = ?
                      WHERE u.company_id = ? AND u.is_deleted = 0
                        AND u.email IS NOT NULL AND TRIM(u.email) <> ''
                      ORDER BY 1",
                )
                .bind(SALES_REP_POSITION)
                .bind(company_id)
                .fetch_all(pool)
                .await;
            }
            Reach::Reps(reps) => ids.extend(reps),
        }
    }
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let placeholders = vec!["?"; ids.len()].join(", ");
    let mut query = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT DISTINCT TRIM(email) FROM users
          WHERE company_id = ? AND is_deleted = 0 AND id IN ({placeholders})
            AND email IS NOT NULL AND TRIM(email) <> ''
          ORDER BY 1"
    )))
    .bind(company_id);
    for id in &ids {
        query = query.bind(id);
    }
    query.fetch_all(pool).await
}

/// A company flow with its steps' text; `None` for a personal or missing one.
pub async fn flow(pool: &MySqlPool, company_id: i32, id: i32) -> Snapshot {
    Snapshot::Flow(load_flow(pool, company_id, id).await.unwrap_or_else(|err| {
        lambda_http::tracing::warn!(?err, id, "automation notice: could not read the flow");
        None
    }))
}

async fn load_flow(
    pool: &MySqlPool,
    company_id: i32,
    id: i32,
) -> Result<Option<Flow>, sqlx::Error> {
    let name: Option<String> = sqlx::query_scalar(
        "SELECT name FROM sms_flows
          WHERE id = ? AND company_id = ? AND user_id IS NULL AND deleted_at IS NULL",
    )
    .bind(id)
    .bind(company_id)
    .fetch_optional(pool)
    .await?;
    let Some(name) = name else {
        return Ok(None);
    };
    let steps = crate::routes::sms::flows::steps_by_flow(pool, company_id, &[id])
        .await?
        .remove(&id)
        .unwrap_or_default()
        .into_iter()
        .map(|step| Step {
            channel: match step.channel {
                crate::routes::sms::flows::StepChannel::Email => Channel::Email,
                crate::routes::sms::flows::StepChannel::Sms => Channel::Sms,
            },
            subject: step
                .subject
                .filter(|subject| !subject.trim().is_empty())
                .or(step.template_subject),
            body: step
                .custom_text
                .filter(|text| !text.trim().is_empty())
                .or(step.template_body)
                .unwrap_or_default(),
            delay_minutes: step.delay_minutes,
        })
        .collect();
    Ok(Some(Flow { name, steps }))
}

/// Emails the company's team about the change from `before` to `after`, when
/// it is worth a notice. Errors are logged: the save already happened.
pub async fn notify(state: &AppState, user: &SessionUser, before: &Snapshot, after: &Snapshot) {
    if let Err(err) = try_notify(state, user, before, after).await {
        lambda_http::tracing::warn!(?err, "automation notice not sent");
    }
}

async fn try_notify(
    state: &AppState,
    user: &SessionUser,
    before: &Snapshot,
    after: &Snapshot,
) -> Result<(), String> {
    let branding = crate::routes::emails::signatures::branding(state, user)
        .await
        .map_err(|err| format!("{err:?}"))?;
    let editor = escape_html(&branding.sender_name);
    let Some(notice) = compose(before, after, &editor, &branding) else {
        return Ok(());
    };
    let recipients = recipients(&state.pool, user.company_id, before, after)
        .await
        .map_err(|err| err.to_string())?;
    // One email each: inbound mail keeps one copy per message, so a shared
    // message would reach only one rep's inbox in the app.
    let mut failed = Vec::new();
    for recipient in recipients {
        let email = OutgoingEmail {
            from_name: Some(format!("{} automations", branding.company_name)),
            from_address: common::amazon::email::DEFAULT_NOREPLY_EMAIL_ADDRESS.to_string(),
            to: vec![recipient.clone()],
            cc: Vec::new(),
            bcc: Vec::new(),
            subject: notice.subject.clone(),
            body: notice.html.clone(),
            signature: None,
            reply_to: vec![user.email.clone()],
            in_reply_to: None,
            references: Vec::new(),
            attachments: Vec::new(),
            transactional: true,
        };
        if let Err(err) = state.webhooks.send_email(&email).await {
            failed.push(format!("{recipient}: {err:?}"));
        }
    }
    if failed.is_empty() {
        Ok(())
    } else {
        Err(failed.join("; "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn branding() -> Branding {
        Branding {
            company_name: "Granite Depot".into(),
            logo_url: None,
            sender_name: "Dima".into(),
        }
    }

    fn sms(drip: Option<Drip>, body: &str) -> Template {
        Template {
            channel: Channel::Sms,
            name: "Welcome".into(),
            subject: None,
            body: body.into(),
            drip,
        }
    }

    fn contacted(hours: i32) -> Option<Drip> {
        Some(Drip {
            list: "Sales › Contacted".into(),
            minutes: hours * 60,
            reps: Vec::new(),
        })
    }

    fn notice(before: Option<Template>, after: Option<Template>) -> Option<Notice> {
        compose(
            &Snapshot::Template(before),
            &Snapshot::Template(after),
            "Dima",
            &branding(),
        )
    }

    #[test]
    fn templates_without_automation_send_nothing() {
        assert_eq!(notice(None, Some(sms(None, "Hi"))), None);
        assert_eq!(
            notice(Some(sms(None, "Hi")), Some(sms(None, "Hello"))),
            None
        );
        assert_eq!(
            notice(Some(sms(contacted(2), "Hi")), Some(sms(contacted(2), "Hi"))),
            None
        );
    }

    #[test]
    fn a_new_drip_says_when_and_what() {
        let notice = notice(
            None,
            Some(sms(contacted(26), "Hi {{customer.first_name}}!")),
        )
        .unwrap();
        assert_eq!(
            notice.subject,
            "Automation change · New automatic text: Welcome"
        );
        assert!(
            notice
                .html
                .contains("1 day 2 hours after a deal enters <b>Sales › Contacted</b>")
        );
        assert!(notice.html.contains(">Customer first name</span>!"));
        assert!(notice.html.contains("9:00 AM and 6:00 PM"));
    }

    #[test]
    fn an_edit_lists_what_changed() {
        let notice = notice(
            Some(sms(contacted(2), "Hi")),
            Some(sms(
                Some(Drip {
                    list: "Sales › Quoted".into(),
                    minutes: 0,
                    reps: Vec::new(),
                }),
                "Hello",
            )),
        )
        .unwrap();
        assert!(notice.html.contains("What changed"));
        assert!(notice.html.contains(
            "Now starts when a deal enters <b>Sales › Quoted</b> (was <b>Sales › Contacted</b>)."
        ));
        assert!(
            notice
                .html
                .contains("Now sends <b>right away</b> after that (was 2 hours).")
        );
        assert!(notice.html.contains("The message text changed"));
    }

    #[test]
    fn limiting_a_drip_to_reps_names_them() {
        let mut limited = contacted(2);
        if let Some(drip) = limited.as_mut() {
            drip.reps = vec![
                Rep {
                    id: 4,
                    name: "Masha".into(),
                },
                Rep {
                    id: 5,
                    name: "Dasha".into(),
                },
            ];
        }
        let before = Some(sms(contacted(2), "Hi"));
        let after = Some(sms(limited, "Hi"));
        let notice = notice(before.clone(), after.clone()).unwrap();
        assert!(notice.html.contains(
            "Now for deals of <b>Masha, Dasha</b> (was every sales rep)."
        ));
        assert!(notice.html.contains(", for deals of <b>Masha, Dasha</b>"));
        assert!(matches!(
            reach(&Snapshot::Template(after)),
            Some(Reach::Reps(ids)) if ids == vec![4, 5]
        ));
        assert!(matches!(
            reach(&Snapshot::Template(before)),
            Some(Reach::EveryRep)
        ));
        assert!(reach(&Snapshot::Template(Some(sms(None, "Hi")))).is_none());
    }

    #[test]
    fn turning_a_drip_off_is_announced() {
        let notice = notice(Some(sms(contacted(2), "Hi")), None).unwrap();
        assert_eq!(
            notice.subject,
            "Automation change · Automatic text turned off: Welcome"
        );
        assert!(notice.html.contains("deleted the text template"));
    }

    #[test]
    fn flows_show_their_steps_and_changes() {
        let step = |channel, body: &str, delay_minutes| Step {
            channel,
            subject: None,
            body: body.into(),
            delay_minutes,
        };
        let before = Flow {
            name: "No reply".into(),
            steps: vec![step(Channel::Sms, "Hi", 0)],
        };
        let after = Flow {
            name: "No reply".into(),
            steps: vec![
                step(Channel::Sms, "Hi", 60),
                step(Channel::Email, "<p>Still there?</p>", 2880),
            ],
        };
        let notice = compose(
            &Snapshot::Flow(Some(before)),
            &Snapshot::Flow(Some(after)),
            "Dima",
            &branding(),
        )
        .unwrap();
        assert!(
            notice
                .html
                .contains("Step 1 now waits <b>1 hour</b> (was right away).")
        );
        assert!(notice.html.contains("Step 2 was added."));
        assert!(notice.html.contains("2 days after the previous step"));
        assert!(notice.html.contains("Still there?"));
        assert!(notice.html.contains("never starts on its own"));
    }

    #[test]
    fn durations_read_naturally() {
        assert_eq!(duration(0, false), "right away");
        assert_eq!(duration(30, false), "30 minutes");
        assert_eq!(duration(60, false), "1 hour");
        assert_eq!(duration(1440 * 3 + 90, false), "3 days 1 hour 30 minutes");
    }
}
