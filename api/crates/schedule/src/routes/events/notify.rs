//! Tells the customer about a booked in-home estimate: a text from the
//! office manager's line, else an email from their sales rep.
//!
//! Port of `sendEstimateScheduleNotification`
//! (`app/utils/estimateScheduleNotify.server.ts`), for the booking message.

use crate::routes::company::queries::company_features;
use crate::routes::emails::thread::from_email;
use crate::routes::sms::provider::{SmsProvider, phone10};
use crate::routes::sms::send::Seat;
use crate::state::AppState;
use crate::webhooks_client::{OutgoingEmail, OutgoingSms};
use chrono::{DateTime, Utc};
use chrono_tz::America::Indiana::Indianapolis;
use lambda_http::tracing;
use sqlx::MySqlPool;
use std::fmt::Write;

/// Sender when neither the rep nor the booker can be found.
const NO_REPLY: &str = "noreply@granite-manager.com";
/// `positions.id` of an office manager.
const OFFICE_MANAGER_POSITION: i32 = 10;

/// How the customer was told.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Channel {
    Sms,
    Email,
}

impl Channel {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Sms => "sms",
            Self::Email => "email",
        }
    }
}

/// Which message: the booking itself, or a reminder before it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotifyKind {
    Scheduled,
    /// A day before.
    Day,
    /// An hour before.
    Hour,
}

impl NotifyKind {
    fn subject(self, company_name: &str) -> String {
        match self {
            Self::Scheduled => format!("{company_name} - In-Home Estimate Scheduled"),
            Self::Day => format!("{company_name} - In-Home Estimate Reminder"),
            Self::Hour => format!("{company_name} - In-Home Estimate Starting Soon"),
        }
    }
}

/// The booking to tell the customer about.
pub struct Booking<'a> {
    pub kind: NotifyKind,
    pub company_id: i32,
    pub customer_id: i32,
    pub customer_name: &'a str,
    /// Who booked it (signs the text).
    pub user_name: Option<&'a str>,
    pub user_id: i32,
    /// An address typed in the booking form, else the customer's first one.
    pub customer_email: Option<&'a str>,
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
    pub location: Option<&'a str>,
}

/// `Tuesday, January 15 at 10:00 AM – 11:00 AM` in store time.
pub fn appointment_when(start: DateTime<Utc>, end: DateTime<Utc>) -> String {
    let start = start.with_timezone(&Indianapolis);
    let end = end.with_timezone(&Indianapolis);
    format!(
        "{} at {} – {}",
        start.format("%A, %B %-d"),
        start.format("%-I:%M %p"),
        end.format("%-I:%M %p")
    )
}

fn first_name(full: &str) -> &str {
    full.split_whitespace().next().unwrap_or("there")
}

/// The confirmation (or reminder) text.
pub fn booking_message(
    kind: NotifyKind,
    user_name: &str,
    user_phone: Option<&str>,
    customer_name: &str,
    when: &str,
    location: Option<&str>,
) -> String {
    let customer = first_name(customer_name);
    let user = first_name(user_name);
    let mut message = match kind {
        NotifyKind::Scheduled => {
            format!("Hi {customer}, your in-home estimate with {user} is scheduled for {when}.")
        }
        NotifyKind::Day => format!(
            "Hi {customer}, this is a reminder that your in-home estimate with {user} is scheduled for {when}."
        ),
        NotifyKind::Hour => {
            format!(
                "Hi {customer}, your in-home estimate with {user} starts in about 1 hour ({when})."
            )
        }
    };
    if let Some(phone) = user_phone.map(str::trim).filter(|phone| !phone.is_empty()) {
        let _ = write!(message, " Contact {user} at {phone}.");
    }
    if let Some(location) = location
        .map(str::trim)
        .filter(|location| !location.is_empty() && kind == NotifyKind::Scheduled)
    {
        let _ = write!(message, " Address: {location}.");
    }
    message
}

/// `317-555-0100` for 10 (or 1 + 10) digits; anything else as given.
pub fn display_phone(phone: &str) -> String {
    let digits: String = phone.chars().filter(char::is_ascii_digit).collect();
    let ten = match digits.len() {
        10 => &digits[..],
        11 if digits.starts_with('1') => &digits[1..],
        _ => return phone.to_string(),
    };
    format!("{}-{}-{}", &ten[..3], &ten[3..6], &ten[6..])
}

/// The customer's phone, then their second phone.
pub async fn customer_phone(
    pool: &MySqlPool,
    company_id: i32,
    customer_id: i32,
) -> Result<Option<String>, sqlx::Error> {
    let row: Option<(Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT phone, phone_2 FROM customers WHERE id = ? AND company_id = ? AND deleted_at IS NULL",
    )
    .bind(customer_id)
    .bind(company_id)
    .fetch_optional(pool)
    .await?;
    Ok(row.and_then(|phones| {
        <[Option<String>; 2]>::from(phones)
            .into_iter()
            .flatten()
            .map(|phone| phone.trim().to_string())
            .find(|phone| !phone.is_empty())
    }))
}

/// The customer's primary email, else their oldest.
pub async fn customer_email(
    pool: &MySqlPool,
    customer_id: i32,
) -> Result<Option<String>, sqlx::Error> {
    let emails: Vec<String> = sqlx::query_scalar(
        "SELECT ce.email FROM customers_emails ce JOIN customers c ON c.id = ce.customer_id
          WHERE ce.customer_id = ? ORDER BY ce.id = c.email_id DESC, ce.created_at, ce.id",
    )
    .bind(customer_id)
    .fetch_all(pool)
    .await?;
    Ok(emails
        .into_iter()
        .map(|email| email.trim().to_string())
        .find(|email| !email.is_empty()))
}

#[derive(sqlx::FromRow)]
pub struct OfficeManager {
    pub id: i32,
    pub name: Option<String>,
    pub agent_id: String,
}

pub async fn office_manager(
    pool: &MySqlPool,
    company_id: i32,
    provider: SmsProvider,
) -> Result<Option<OfficeManager>, sqlx::Error> {
    let (Some(agent), Some(phone)) = (provider.agent_column(), provider.phone_column()) else {
        // Telnyx: an office manager with a line, while the company texts
        // through Telnyx; their user id is their agent id there.
        if crate::phone::texting::texting_setup(pool, company_id)
            .await?
            .is_none()
        {
            return Ok(None);
        }
        return sqlx::query_as(
            "SELECT u.id, u.name, CAST(u.id AS CHAR) AS agent_id FROM users u
               JOIN users_positions up ON up.user_id = u.id
              WHERE up.company_id = ? AND up.position_id = ? AND u.is_deleted = 0
                AND EXISTS (SELECT 1 FROM telnyx_number_users nu
                              JOIN telnyx_numbers n ON n.id = nu.number_id AND n.company_id = up.company_id
                             WHERE nu.user_id = u.id AND nu.can_call_from <> 0 AND n.active <> 0)
              ORDER BY u.name LIMIT 1",
        )
        .bind(company_id)
        .bind(OFFICE_MANAGER_POSITION)
        .fetch_optional(pool)
        .await;
    };
    sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT u.id, u.name, TRIM(u.{agent}) AS agent_id FROM users u
           JOIN users_positions up ON up.user_id = u.id
          WHERE up.company_id = ? AND up.position_id = ? AND u.is_deleted = 0
            AND TRIM(COALESCE(u.{agent}, '')) <> '' AND TRIM(COALESCE(u.{phone}, '')) <> ''
          ORDER BY u.name LIMIT 1"
    )))
    .bind(company_id)
    .bind(OFFICE_MANAGER_POSITION)
    .fetch_optional(pool)
    .await
}

/// How an office manager texts on `provider`: on Telnyx from their line
/// (`None` without one).
pub async fn manager_seat(
    pool: &MySqlPool,
    company_id: i32,
    provider: SmsProvider,
    manager: &OfficeManager,
) -> Result<Option<Seat>, sqlx::Error> {
    if provider == SmsProvider::Telnyx {
        return Ok(
            crate::phone::texting::user_line(pool, company_id, manager.id)
                .await?
                .map(|line| Seat {
                    agent_id: manager.agent_id.clone(),
                    line: Some(line),
                }),
        );
    }
    Ok(Some(Seat {
        agent_id: manager.agent_id.clone(),
        line: None,
    }))
}

/// Who texts the company's notices (bookings, surveys): an office manager
/// with a Telnyx line while the company texts through Telnyx, else one with
/// a line on the company's provider; with their provider and seat.
pub async fn notice_sender(
    pool: &MySqlPool,
    company_id: i32,
) -> Result<Option<(SmsProvider, OfficeManager, Seat)>, sqlx::Error> {
    let features = company_features(pool, company_id).await?;
    let mut providers = Vec::new();
    if features.telnyx_sms {
        providers.push(SmsProvider::Telnyx);
    }
    providers.extend(features.sms_provider());
    for provider in providers {
        let Some(manager) = office_manager(pool, company_id, provider).await? else {
            continue;
        };
        if let Some(seat) = manager_seat(pool, company_id, provider, &manager).await? {
            return Ok(Some((provider, manager, seat)));
        }
    }
    Ok(None)
}

/// The customer's sales rep's phone, else the booker's.
async fn contact_phone(
    pool: &MySqlPool,
    booking: &Booking<'_>,
) -> Result<Option<String>, sqlx::Error> {
    let rep: Option<Option<String>> = sqlx::query_scalar(
        "SELECT u.phone_number FROM customers c JOIN users u ON u.id = c.sales_rep
          WHERE c.id = ? AND c.company_id = ? AND c.deleted_at IS NULL AND u.is_deleted = 0 LIMIT 1",
    )
    .bind(booking.customer_id)
    .bind(booking.company_id)
    .fetch_optional(pool)
    .await?;
    let booker: Option<Option<String>> =
        match rep.clone().flatten() {
            Some(_) => None,
            None => sqlx::query_scalar(
                "SELECT phone_number FROM users WHERE id = ? AND company_id = ? AND is_deleted = 0",
            )
            .bind(booking.user_id)
            .bind(booking.company_id)
            .fetch_optional(pool)
            .await?,
        };
    Ok(rep
        .flatten()
        .or_else(|| booker.flatten())
        .map(|phone| display_phone(phone.trim()))
        .filter(|phone| !phone.is_empty()))
}

/// The sales rep's (else the booker's) name and sending address.
async fn email_sender(
    pool: &MySqlPool,
    booking: &Booking<'_>,
) -> Result<(Option<String>, String), sqlx::Error> {
    sender_for(
        pool,
        booking.company_id,
        booking.customer_id,
        booking.user_id,
    )
    .await
}

/// The customer's sales rep's (else `user_id`'s) name and sending address.
pub async fn sender_for(
    pool: &MySqlPool,
    company_id: i32,
    customer_id: i32,
    user_id: i32,
) -> Result<(Option<String>, String), sqlx::Error> {
    type Sender = (String, Option<String>, Option<String>);
    let rep: Option<Sender> = sqlx::query_as(
        "SELECT u.email, u.email_name, co.domain FROM customers c
           JOIN users u ON u.id = c.sales_rep AND u.is_deleted = 0
           JOIN company co ON co.id = c.company_id
          WHERE c.id = ? AND c.company_id = ? AND c.deleted_at IS NULL LIMIT 1",
    )
    .bind(customer_id)
    .bind(company_id)
    .fetch_optional(pool)
    .await?;
    let sender = match rep {
        Some(rep) => Some(rep),
        None => {
            sqlx::query_as(
                "SELECT u.email, u.email_name, co.domain FROM users u JOIN company co ON co.id = u.company_id
                  WHERE u.id = ? AND u.company_id = ? AND u.is_deleted = 0",
            )
            .bind(user_id)
            .bind(company_id)
            .fetch_optional(pool)
            .await?
        }
    };
    Ok(sender.map_or_else(
        || (None, NO_REPLY.to_string()),
        |(email, name, domain)| {
            (
                name.filter(|name| !name.trim().is_empty()),
                from_email(domain.as_deref(), &email),
            )
        },
    ))
}

pub fn escape_html(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// Texts, else emails, the customer; `None` when neither went out.
pub async fn notify_booking(
    state: &AppState,
    booking: &Booking<'_>,
) -> Result<Option<Channel>, sqlx::Error> {
    let pool = &state.pool;
    let company_name: Option<String> = sqlx::query_scalar("SELECT name FROM company WHERE id = ?")
        .bind(booking.company_id)
        .fetch_optional(pool)
        .await?;
    let company_name = company_name.unwrap_or_else(|| "Our Company".to_string());
    let sender = notice_sender(pool, booking.company_id).await?;
    let user_name = booking
        .user_name
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_string)
        .or_else(|| {
            sender
                .as_ref()
                .and_then(|(_, m, _)| m.name.clone())
                .filter(|n| !n.trim().is_empty())
        })
        .unwrap_or_else(|| company_name.clone());
    let phone = contact_phone(pool, booking).await?;
    let text = booking_message(
        booking.kind,
        &user_name,
        phone.as_deref(),
        booking.customer_name,
        &appointment_when(booking.start, booking.end),
        booking.location,
    );

    if let Some((provider, manager, seat)) = &sender {
        let digits = customer_phone(pool, booking.company_id, booking.customer_id)
            .await?
            .map(|phone| phone10(&phone))
            .filter(|digits| digits.len() == 10);
        if let Some(phone_digits) = digits {
            let sms = OutgoingSms {
                provider: provider.as_str().to_string(),
                company_id: booking.company_id,
                user_id: manager.id,
                agent_id: manager.agent_id.clone(),
                phone_digits,
                text: text.clone(),
                attachments: Vec::new(),
                is_automated: true,
                idempotency_key: None,
            };
            match crate::routes::sms::send::send_text(state, *provider, seat, &sms).await {
                Ok(_) => return Ok(Some(Channel::Sms)),
                Err(failure) => {
                    tracing::warn!(error = %failure.error, "estimate booking text failed");
                }
            }
        }
    }

    let email = match booking
        .customer_email
        .map(str::trim)
        .filter(|email| !email.is_empty())
    {
        Some(email) => Some(email.to_string()),
        None => customer_email(pool, booking.customer_id).await?,
    };
    let Some(email) = email else {
        return Ok(None);
    };
    let (from_name, from_address) = email_sender(pool, booking).await?;
    let message = OutgoingEmail {
        from_name,
        from_address,
        to: vec![email],
        cc: Vec::new(),
        bcc: Vec::new(),
        subject: booking.kind.subject(&company_name),
        body: format!("<p>{}</p>", escape_html(&text)),
        signature: None,
        reply_to: Vec::new(),
        in_reply_to: None,
        references: Vec::new(),
        attachments: Vec::new(),
        transactional: true,
    };
    match state.webhooks.send_email(&message).await {
        Ok(_) => Ok(Some(Channel::Email)),
        Err(err) => {
            tracing::warn!(error = ?err, "estimate booking email failed");
            Ok(None)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_booking_text_reads_like_the_remix_one() {
        let start = "2030-01-15T15:00:00Z".parse().unwrap();
        let end = "2030-01-15T16:00:00Z".parse().unwrap();
        let when = appointment_when(start, end);
        assert_eq!(when, "Tuesday, January 15 at 10:00 AM – 11:00 AM");
        assert_eq!(
            booking_message(
                NotifyKind::Scheduled,
                "Rita Rep",
                Some("317-555-0100"),
                "Alice Lead",
                &when,
                Some(" 1 Main St ")
            ),
            "Hi Alice, your in-home estimate with Rita is scheduled for Tuesday, January 15 at 10:00 AM – 11:00 AM. Contact Rita at 317-555-0100. Address: 1 Main St."
        );
        assert_eq!(
            booking_message(NotifyKind::Scheduled, "Rita", None, "", &when, None),
            "Hi there, your in-home estimate with Rita is scheduled for Tuesday, January 15 at 10:00 AM – 11:00 AM."
        );
        assert_eq!(
            booking_message(
                NotifyKind::Hour,
                "Rita",
                None,
                "Alice",
                &when,
                Some("1 Main St")
            ),
            "Hi Alice, your in-home estimate with Rita starts in about 1 hour (Tuesday, January 15 at 10:00 AM – 11:00 AM)."
        );
    }

    #[test]
    fn phones_are_shown_dashed() {
        assert_eq!(display_phone("(317) 555-0100"), "317-555-0100");
        assert_eq!(display_phone("+1 317 555 0100"), "317-555-0100");
        assert_eq!(display_phone("555-01"), "555-01");
    }
}
