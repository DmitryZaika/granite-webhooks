//! Who an incoming call rings, and what follows when nobody answers.
//!
//! A call rings the number's ringing users who are live, in the number's
//! company, still an employee or admin, and available now (not
//! Do not disturb, inside their working hours); for the main line only while
//! the company is open (business hours, not a holiday). Their signed-in
//! phones with a SIP user ring together; the verified cells of those with
//! cell fallback on are rung after them, behind a "press 1" screen. A call
//! nobody answers then takes the number's no-answer action: the AI
//! Assistant (voicemail when the company has none), voicemail, or a forward.
//!
//! Nothing rings while the company's calling is switched off (or the
//! company is deleted) or the number is inactive: such calls go straight to
//! the no-answer action.

use super::company::{CompanyConfig, time_zone_of};
use super::devices::LIVE_DEVICE;
use super::lines::NumberKind;
use super::numbers::{is_nanp, to_e164};
use super::settings::{self, DEFAULT_RING_TIMEOUT_SECS, RING_TIMEOUT_SECS, UserSettings};
use super::wire_enum;
use chrono::{DateTime, Utc};
use sqlx::{FromRow, MySqlPool, QueryBuilder};

/// What callers hear before leaving a voicemail when the number has no
/// greeting of its own.
pub const DEFAULT_GREETING: &str =
    "Sorry, nobody can take your call right now. Please leave a message after the tone.";

wire_enum! {
    /// What a call nobody answers does: `ai` (the AI Assistant),
    /// `voicemail` or `forward` (to another number).
    pub enum NoAnswerAction {
        /// The company's AI Assistant takes the call.
        Ai = "ai",
        /// The caller leaves a voicemail.
        Voicemail = "voicemail",
        /// The call is forwarded to `forward_to`.
        Forward = "forward",
    }
}

/// A company number, as calls to it are routed.
#[derive(Debug, Clone, PartialEq, Eq, FromRow)]
pub struct Line {
    pub id: i32,
    pub company_id: i32,
    /// E.164.
    pub e164: String,
    #[sqlx(try_from = "String")]
    pub kind: NumberKind,
    #[sqlx(try_from = "String")]
    pub no_answer_action: NoAnswerAction,
    pub forward_to: Option<String>,
    /// What callers hear before a voicemail.
    pub greeting: Option<String>,
    pub active: bool,
}

const LINE_COLUMNS: &str = "SELECT id, company_id, e164, CAST(kind AS CHAR) AS kind,
        CAST(no_answer_action AS CHAR) AS no_answer_action, forward_to, greeting,
        active <> 0 AS active
   FROM telnyx_numbers";

/// The number `e164`, whatever company it belongs to.
pub async fn line_by_e164(pool: &MySqlPool, e164: &str) -> sqlx::Result<Option<Line>> {
    sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "{LINE_COLUMNS} WHERE e164 = ?"
    )))
    .bind(e164)
    .fetch_optional(pool)
    .await
}

/// A number of the company.
pub async fn line(pool: &MySqlPool, company_id: i32, id: i32) -> sqlx::Result<Option<Line>> {
    sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "{LINE_COLUMNS} WHERE id = ? AND company_id = ?"
    )))
    .bind(id)
    .bind(company_id)
    .fetch_optional(pool)
    .await
}

/// A user an incoming call rings now, with their phone settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ringer {
    pub user_id: i32,
    pub settings: UserSettings,
}

/// A signed-in phone that rings.
#[derive(Debug, Clone, PartialEq, Eq, FromRow)]
pub struct RingPhone {
    pub device_id: i64,
    pub user_id: i32,
    pub sip_username: String,
}

/// The company is open at `now` (for its main line): inside its business
/// hours, when it set any, and not on one of its holidays (local dates in
/// the company time zone).
pub fn open(config: &CompanyConfig, now: DateTime<Utc>) -> bool {
    let today = now
        .with_timezone(&time_zone_of(Some(config)))
        .date_naive()
        .format("%Y-%m-%d")
        .to_string();
    if config.holidays.iter().any(|day| day.trim() == today) {
        return false;
    }
    config
        .business_hours
        .as_ref()
        .is_none_or(|hours| hours.contains(now))
}

/// SQL condition: the user `u` may use Granite Phone (an employee, admin or
/// superuser, or a super admin by position).
pub(crate) const PHONE_USER: &str =
    "(COALESCE(u.is_employee, 0) <> 0 OR COALESCE(u.is_admin, 0) <> 0
      OR COALESCE(u.is_superuser, 0) <> 0
      OR EXISTS (SELECT 1 FROM users_positions sp WHERE sp.user_id = u.id AND sp.position_id = 9))";

/// The users a call to `line` rings at `now`, by id.
pub async fn ringers(
    pool: &MySqlPool,
    config: &CompanyConfig,
    line: &Line,
    now: DateTime<Utc>,
) -> sqlx::Result<Vec<Ringer>> {
    let ours = line.company_id == config.company_id;
    if !(ours && config.calling_on() && line.active) {
        return Ok(Vec::new());
    }
    if line.kind == NumberKind::Main && !open(config, now) {
        return Ok(Vec::new());
    }
    let users: Vec<i32> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT u.id FROM telnyx_number_users nu
           JOIN telnyx_numbers n ON n.id = nu.number_id
           JOIN users u ON u.id = nu.user_id AND u.company_id = n.company_id AND u.is_deleted = 0
          WHERE nu.number_id = ? AND n.company_id = ? AND nu.rings <> 0 AND {PHONE_USER}
          ORDER BY u.id"
    )))
    .bind(line.id)
    .bind(config.company_id)
    .fetch_all(pool)
    .await?;
    let mut ringers = Vec::with_capacity(users.len());
    for user_id in users {
        let settings = settings::load(pool, user_id, config.company_id).await?;
        if settings.availability(now).rings() {
            ringers.push(Ringer { user_id, settings });
        }
    }
    Ok(ringers)
}

/// The signed-in phones of `user_ids` in the company that can ring (with a
/// SIP user), by id. A browser phone (the CRM's phone dock) rings only
/// while its tab says it is connected to Telnyx (`online_until`); app
/// phones always (Telnyx wakes them with its own push).
pub async fn phones(
    pool: &MySqlPool,
    company_id: i32,
    user_ids: &[i32],
) -> sqlx::Result<Vec<RingPhone>> {
    if user_ids.is_empty() {
        return Ok(Vec::new());
    }
    let mut query = QueryBuilder::new(format!(
        "SELECT CAST(pd.id AS SIGNED) AS device_id, pd.user_id, pd.sip_username
           FROM phone_devices pd
           JOIN device_sessions ds ON ds.id = pd.device_session_id
          WHERE {LIVE_DEVICE} AND pd.sip_username IS NOT NULL AND ds.company_id = pd.company_id
            AND (pd.platform <> 'web' OR pd.online_until > UTC_TIMESTAMP())
            AND pd.company_id = "
    ));
    query.push_bind(company_id);
    crate::sql::push_in(&mut query, "pd.user_id", user_ids);
    query.push(" ORDER BY pd.id");
    query.build_query_as().fetch_all(pool).await
}

/// How long phones ring: the longest ring timeout the users chose (15 to
/// 60 seconds; 25 when there is nobody).
pub fn ring_timeout(ringers: &[Ringer]) -> i64 {
    ringers
        .iter()
        .map(|ringer| ringer.settings.ring_timeout_secs)
        .max()
        .unwrap_or(DEFAULT_RING_TIMEOUT_SECS)
        .clamp(*RING_TIMEOUT_SECS.start(), *RING_TIMEOUT_SECS.end())
}

/// A verified cell rung after the phones: (user, cell, E.164).
pub fn cells(ringers: &[Ringer]) -> Vec<(i32, String)> {
    ringers
        .iter()
        .filter(|ringer| ringer.settings.cell_fallback)
        .filter_map(|ringer| Some((ringer.user_id, ringer.settings.cell_number.clone()?)))
        .collect()
}

/// What a call nobody answered does next.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NoAnswer {
    /// Ring these cells (user, E.164), each behind "press 1".
    Cells(Vec<(i32, String)>),
    /// Connect the AI Assistant with this id.
    Ai(String),
    /// Take a voicemail after this greeting.
    Voicemail(String),
    /// Forward to this number (E.164, US or Canada).
    Forward(String),
}

/// The next step for a call to `line` nobody answered.
///
/// Its users' `cells` first, unless they were tried already or the call was
/// declined; then the number's action. The AI Assistant needs the company's
/// assistant id, and a forward a US or Canada number; else the caller
/// leaves a voicemail.
pub fn no_answer(
    line: &Line,
    config: &CompanyConfig,
    cells: Vec<(i32, String)>,
    cells_tried: bool,
    declined: bool,
) -> NoAnswer {
    if !cells_tried && !declined && !cells.is_empty() {
        return NoAnswer::Cells(cells);
    }
    let voicemail = || {
        NoAnswer::Voicemail(
            line.greeting
                .as_deref()
                .map(str::trim)
                .filter(|greeting| !greeting.is_empty())
                .unwrap_or(DEFAULT_GREETING)
                .to_string(),
        )
    };
    match line.no_answer_action {
        NoAnswerAction::Ai => config
            .ai_assistant_id
            .clone()
            .map_or_else(voicemail, NoAnswer::Ai),
        NoAnswerAction::Voicemail => voicemail(),
        NoAnswerAction::Forward => line
            .forward_to
            .as_deref()
            .and_then(to_e164)
            .filter(|number| is_nanp(number))
            .map_or_else(voicemail, NoAnswer::Forward),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::phone::availability::{WorkingDay, WorkingHours};
    use crate::phone::company::RecordingMode;

    fn config() -> CompanyConfig {
        CompanyConfig {
            company_id: 100,
            enabled: true,
            company_deleted: false,
            webhook_token: "t".into(),
            credential_connection_id: None,
            texml_app_id: None,
            outbound_voice_profile_id: None,
            billing_group_id: None,
            telnyx_account_id: None,
            ai_assistant_id: Some("assistant-1".into()),
            fallback_texml_url: None,
            recording_mode: RecordingMode::Inbound,
            recording_notice_url: None,
            messaging_profile_id: None,
            business_hours: None,
            holidays: Vec::new(),
            min_app_version: None,
            daily_call_limit: 300,
        }
    }

    fn line(action: NoAnswerAction, forward_to: Option<&str>) -> Line {
        Line {
            id: 1,
            company_id: 100,
            e164: "+13175550150".into(),
            kind: NumberKind::Main,
            no_answer_action: action,
            forward_to: forward_to.map(str::to_string),
            greeting: None,
            active: true,
        }
    }

    fn utc(text: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(text)
            .unwrap()
            .with_timezone(&Utc)
    }

    #[test]
    fn the_main_line_closes_outside_hours_and_on_holidays() {
        let mut config = config();
        assert!(open(&config, utc("2026-10-14T03:00:00Z")));
        config.business_hours = Some(WorkingHours {
            tz: "America/Indiana/Indianapolis".into(),
            days: vec![WorkingDay {
                day: 3,
                start: "08:00".into(),
                end: "17:00".into(),
            }],
        });
        // Wednesday 2026-10-14, 10:00 and 20:00 in Indianapolis.
        assert!(open(&config, utc("2026-10-14T14:00:00Z")));
        assert!(!open(&config, utc("2026-10-15T00:00:00Z")));
        // A holiday is a local date: 2026-10-14 there.
        config.holidays = vec!["2026-10-14".into()];
        assert!(!open(&config, utc("2026-10-14T14:00:00Z")));
    }

    #[test]
    fn nobody_answered_cells_then_the_numbers_action() {
        let config = config();
        let cells = vec![(100, "+13175550177".to_string())];
        let ai = line(NoAnswerAction::Ai, None);
        assert_eq!(
            no_answer(&ai, &config, cells.clone(), false, false),
            NoAnswer::Cells(cells.clone())
        );
        assert_eq!(
            no_answer(&ai, &config, cells.clone(), true, false),
            NoAnswer::Ai("assistant-1".into())
        );
        assert_eq!(
            no_answer(&ai, &config, cells, false, true),
            NoAnswer::Ai("assistant-1".into())
        );
        let no_ai = CompanyConfig {
            ai_assistant_id: None,
            ..config.clone()
        };
        assert_eq!(
            no_answer(&ai, &no_ai, Vec::new(), false, false),
            NoAnswer::Voicemail(DEFAULT_GREETING.into())
        );
        let mut voicemail = line(NoAnswerAction::Voicemail, None);
        voicemail.greeting = Some(" Leave a message for Acme. ".into());
        assert_eq!(
            no_answer(&voicemail, &config, Vec::new(), false, false),
            NoAnswer::Voicemail("Leave a message for Acme.".into())
        );
        assert_eq!(
            no_answer(
                &line(NoAnswerAction::Forward, Some("317-555-0188")),
                &config,
                Vec::new(),
                false,
                false
            ),
            NoAnswer::Forward("+13175550188".into())
        );
        // Abroad or unreadable: voicemail instead.
        assert_eq!(
            no_answer(
                &line(NoAnswerAction::Forward, Some("+442079460958")),
                &config,
                Vec::new(),
                false,
                false
            ),
            NoAnswer::Voicemail(DEFAULT_GREETING.into())
        );
    }
}
