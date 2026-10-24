//! A user's incoming call rules (`phone_call_rules`, the app's "+ Add rule"),
//! "Forward all calls", and what they decide for one incoming call.
//!
//! What a user's part of an incoming call is ([`decide`]), in this order:
//!
//! 1. Do not disturb: nothing of theirs (their phones do not ring, no rule
//!    runs; the number's no-answer action takes the call as before).
//! 2. Forward all calls: to voicemail or to a number.
//! 3. The first enabled custom rule that matches (callers, schedule, the
//!    number called): ring my phones (even outside working hours), forward,
//!    voicemail, or a message and hang up.
//! 4. Working hours: inside them the phones ring, outside nothing.
//!
//! A decision other than ringing takes the whole call only on the user's
//! own direct line (a direct number that rings nobody else): see
//! `routing::route`. On shared and main lines it only keeps that user's
//! phones from ringing, so one person's forward or rule never takes a call
//! away from the others it rings.

use super::availability::{AvailabilityStatus, WorkingHours};
use super::numbers::{is_nanp, last10, to_e164};
use super::settings::UserSettings;
use super::wire_enum;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, MySqlPool, QueryBuilder};
use std::collections::HashMap;
use utoipa::ToSchema;

/// Rules a user may have.
pub const MAX_RULES: usize = 20;
/// Callers one rule may name.
pub const MAX_CALLERS: usize = 50;
/// The longest rule name and message.
pub const MAX_NAME_CHARS: usize = 100;
pub const MAX_MESSAGE_CHARS: usize = 500;

wire_enum! {
    /// What a matching rule does with the call: `ring` (my phones),
    /// `forward` (to `forwardTo`), `voicemail`, or `message` (play
    /// `message`, then hang up).
    pub enum RuleAction {
        /// Ring my phones (and my cell, with cell fallback), even outside
        /// my working hours.
        Ring = "ring",
        /// Forward the call to a US or Canada number.
        Forward = "forward",
        /// Send the caller to voicemail.
        Voicemail = "voicemail",
        /// Play a message to the caller, then hang up.
        Message = "message",
    }
}

wire_enum! {
    /// Where "Forward all calls" sends calls: `voicemail` or `number`.
    pub enum ForwardAllTo {
        /// The caller leaves a voicemail.
        Voicemail = "voicemail",
        /// The call is forwarded to `forwardAllNumber`.
        Number = "number",
    }
}

/// One caller a rule is for: a phone number or a CRM contact (whichever
/// of their numbers calls).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct RuleCaller {
    /// A phone number, E.164; null for a contact.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub number: Option<String>,
    /// A customer of the company (`GET /v1/phone/contacts/{id}`); null for a
    /// number.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub customer_id: Option<i32>,
}

/// A custom incoming call rule.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct CallRule {
    /// The rule's id.
    pub id: i64,
    /// Its name, e.g. "After hours".
    pub name: String,
    /// The rule's switch: off rules are kept but never match.
    pub enabled: bool,
    /// The callers it is for; null = any caller.
    pub callers: Option<Vec<RuleCaller>>,
    /// When it applies, as weekly local time ranges; null = any time.
    pub schedule: Option<WorkingHours>,
    /// Which of my numbers was called (`numberId`s of my caller IDs and the
    /// lines that ring me); null = any of them.
    pub number_ids: Option<Vec<i32>>,
    /// What it does.
    pub action: RuleAction,
    /// With `forward`: where to, E.164; else null.
    pub forward_to: Option<String>,
    /// With `message`: what the caller hears before the call ends; else
    /// null.
    pub message: Option<String>,
}

#[derive(FromRow)]
struct RuleRow {
    id: i64,
    user_id: i32,
    name: String,
    enabled: bool,
    callers: Option<String>,
    schedule: Option<String>,
    number_ids: Option<String>,
    action: String,
    forward_to: Option<String>,
    message: Option<String>,
}

impl RuleRow {
    fn rule(self) -> Option<CallRule> {
        Some(CallRule {
            id: self.id,
            name: self.name,
            enabled: self.enabled,
            callers: self
                .callers
                .as_deref()
                .and_then(|json| serde_json::from_str(json).ok()),
            schedule: self.schedule.as_deref().and_then(WorkingHours::from_json),
            number_ids: self
                .number_ids
                .as_deref()
                .and_then(|json| serde_json::from_str(json).ok()),
            action: RuleAction::parse(&self.action)?,
            forward_to: self.forward_to,
            message: self.message,
        })
    }
}

const RULE_COLUMNS: &str =
    "SELECT CAST(id AS SIGNED) AS id, user_id, name, enabled <> 0 AS enabled,
        CAST(callers AS CHAR) AS callers, CAST(schedule AS CHAR) AS schedule,
        CAST(number_ids AS CHAR) AS number_ids, CAST(action AS CHAR) AS action,
        forward_to, message
   FROM phone_call_rules";

/// The user's rules in the company, in the order they are tried.
pub async fn list(pool: &MySqlPool, company_id: i32, user_id: i32) -> sqlx::Result<Vec<CallRule>> {
    let rows: Vec<RuleRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "{RULE_COLUMNS} WHERE company_id = ? AND user_id = ? ORDER BY position, id"
    )))
    .bind(company_id)
    .bind(user_id)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().filter_map(RuleRow::rule).collect())
}

/// The enabled rules of `user_ids` in the company, by user, in order.
pub async fn enabled_for(
    pool: &MySqlPool,
    company_id: i32,
    user_ids: &[i32],
) -> sqlx::Result<HashMap<i32, Vec<CallRule>>> {
    let mut by_user: HashMap<i32, Vec<CallRule>> = HashMap::new();
    if user_ids.is_empty() {
        return Ok(by_user);
    }
    let mut query = QueryBuilder::new(format!(
        "{RULE_COLUMNS} WHERE enabled <> 0 AND company_id = "
    ));
    query.push_bind(company_id);
    crate::sql::push_in(&mut query, "user_id", user_ids);
    query.push(" ORDER BY user_id, position, id");
    let rows: Vec<RuleRow> = query.build_query_as().fetch_all(pool).await?;
    for row in rows {
        let user_id = row.user_id;
        if let Some(rule) = row.rule() {
            by_user.entry(user_id).or_default().push(rule);
        }
    }
    Ok(by_user)
}

/// The customers rules name, among `rules`.
pub fn named_customers<'a>(rules: impl IntoIterator<Item = &'a CallRule>) -> Vec<i32> {
    let mut ids: Vec<i32> = rules
        .into_iter()
        .flat_map(|rule| rule.callers.iter().flatten())
        .filter_map(|caller| caller.customer_id)
        .collect();
    ids.sort_unstable();
    ids.dedup();
    ids
}

/// Which of the company's `customer_ids` have `caller_last10` as one of
/// their numbers (deleted customers never match).
pub async fn customers_with_number(
    pool: &MySqlPool,
    company_id: i32,
    customer_ids: &[i32],
    caller_last10: Option<&str>,
) -> sqlx::Result<Vec<i32>> {
    let Some(caller) = caller_last10 else {
        return Ok(Vec::new());
    };
    if customer_ids.is_empty() {
        return Ok(Vec::new());
    }
    let mut query = QueryBuilder::new(
        "SELECT c.id, c.phone, c.phone_2 FROM customers c
          WHERE c.deleted_at IS NULL AND c.company_id = ",
    );
    query.push_bind(company_id);
    crate::sql::push_in(&mut query, "c.id", customer_ids);
    let rows: Vec<(i32, Option<String>, Option<String>)> =
        query.build_query_as().fetch_all(pool).await?;
    Ok(rows
        .into_iter()
        .filter(|(_, phone, phone_2)| {
            super::contacts::index_phones(phone.as_deref(), phone_2.as_deref())
                .iter()
                .any(|digits| digits == caller)
        })
        .map(|(id, _, _)| id)
        .collect())
}

/// An incoming call, as rules see it.
#[derive(Debug, Clone, Copy)]
pub struct Incoming<'a> {
    /// The caller's last 10 digits; None for a hidden or unreadable number.
    pub caller_last10: Option<&'a str>,
    /// The customers (named by some rule) the caller's number belongs to.
    pub caller_customers: &'a [i32],
    /// The company number called.
    pub number_id: i32,
    pub now: DateTime<Utc>,
}

impl CallRule {
    /// The rule matches the call (each condition it sets holds).
    pub fn matches(&self, call: &Incoming<'_>) -> bool {
        if !self.enabled {
            return false;
        }
        if let Some(callers) = &self.callers {
            let named = callers.iter().any(|caller| {
                let by_number = caller
                    .number
                    .as_deref()
                    .and_then(last10)
                    .is_some_and(|digits| Some(digits.as_str()) == call.caller_last10);
                let by_contact = caller
                    .customer_id
                    .is_some_and(|id| call.caller_customers.contains(&id));
                by_number || by_contact
            });
            if !named {
                return false;
            }
        }
        if let Some(schedule) = &self.schedule
            && !schedule.contains(call.now)
        {
            return false;
        }
        if let Some(numbers) = &self.number_ids
            && !numbers.contains(&call.number_id)
        {
            return false;
        }
        true
    }
}

/// What a user's part of an incoming call is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Their phones ring.
    Ring,
    /// Nothing of theirs: Do not disturb, or outside working hours.
    Away,
    /// Forward to this number (E.164, US or Canada).
    Forward(String),
    /// The caller leaves a voicemail.
    Voicemail,
    /// The caller hears this, then the call ends.
    Message(String),
}

/// A forward target that can be dialed: E.164, US or Canada.
pub fn forward_number(text: Option<&str>) -> Option<String> {
    text.and_then(to_e164).filter(|number| is_nanp(number))
}

/// What the user's part of `call` is (see the module docs).
pub fn decide(settings: &UserSettings, rules: &[CallRule], call: &Incoming<'_>) -> Decision {
    let availability = settings.availability(call.now);
    if availability.status == AvailabilityStatus::Dnd {
        return Decision::Away;
    }
    if settings.forward_all {
        return match settings.forward_all_to {
            ForwardAllTo::Number => forward_number(settings.forward_all_number.as_deref())
                .map_or(Decision::Voicemail, Decision::Forward),
            ForwardAllTo::Voicemail => Decision::Voicemail,
        };
    }
    if let Some(rule) = rules.iter().find(|rule| rule.matches(call)) {
        return match rule.action {
            RuleAction::Ring => Decision::Ring,
            RuleAction::Voicemail => Decision::Voicemail,
            RuleAction::Forward => forward_number(rule.forward_to.as_deref())
                .map_or(Decision::Voicemail, Decision::Forward),
            RuleAction::Message => Decision::Message(
                rule.message
                    .as_deref()
                    .map(str::trim)
                    .filter(|message| !message.is_empty())
                    .unwrap_or(DEFAULT_RULE_MESSAGE)
                    .to_string(),
            ),
        };
    }
    if availability.rings() {
        Decision::Ring
    } else {
        Decision::Away
    }
}

/// What a `message` rule says when its message is empty.
pub const DEFAULT_RULE_MESSAGE: &str =
    "Sorry, I can't take your call right now. Please call again later. Goodbye.";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::phone::availability::WorkingDay;

    fn utc(text: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(text)
            .unwrap()
            .with_timezone(&Utc)
    }

    fn rule(action: RuleAction) -> CallRule {
        CallRule {
            id: 1,
            name: "Rule".into(),
            enabled: true,
            callers: None,
            schedule: None,
            number_ids: None,
            action,
            forward_to: None,
            message: None,
        }
    }

    fn call<'a>(caller: Option<&'a str>, customers: &'a [i32]) -> Incoming<'a> {
        Incoming {
            caller_last10: caller,
            caller_customers: customers,
            number_id: 9801,
            // Wednesday 2026-10-14, 10:00 in Indianapolis.
            now: utc("2026-10-14T14:00:00Z"),
        }
    }

    fn evenings() -> WorkingHours {
        WorkingHours {
            tz: "America/Indiana/Indianapolis".into(),
            days: vec![WorkingDay {
                day: 3,
                start: "17:00".into(),
                end: "24:00".into(),
            }],
        }
    }

    #[test]
    fn a_rule_matches_when_every_condition_it_sets_holds() {
        let mut vip = rule(RuleAction::Ring);
        vip.callers = Some(vec![
            RuleCaller {
                number: Some("+13175550100".into()),
                customer_id: None,
            },
            RuleCaller {
                number: None,
                customer_id: Some(1000),
            },
        ]);
        assert!(vip.matches(&call(Some("3175550100"), &[])));
        assert!(vip.matches(&call(Some("3175550199"), &[1000])));
        assert!(!vip.matches(&call(Some("3175550199"), &[1001])));
        assert!(!vip.matches(&call(None, &[])));
        vip.number_ids = Some(vec![9802]);
        assert!(!vip.matches(&call(Some("3175550100"), &[])));
        vip.number_ids = Some(vec![9801, 9802]);
        assert!(vip.matches(&call(Some("3175550100"), &[])));
        vip.schedule = Some(evenings());
        assert!(!vip.matches(&call(Some("3175550100"), &[])));
        let mut evening = call(Some("3175550100"), &[]);
        evening.now = utc("2026-10-14T22:00:00Z");
        assert!(vip.matches(&evening));
        vip.enabled = false;
        assert!(!vip.matches(&evening));
    }

    #[test]
    fn do_not_disturb_then_forward_all_then_rules_then_hours() {
        let now = call(Some("3175550100"), &[]);
        let mut settings = UserSettings::default();
        assert_eq!(decide(&settings, &[], &now), Decision::Ring);
        let rules = [rule(RuleAction::Message)];
        assert_eq!(
            decide(&settings, &rules, &now),
            Decision::Message(DEFAULT_RULE_MESSAGE.into())
        );
        settings.forward_all = true;
        assert_eq!(decide(&settings, &rules, &now), Decision::Voicemail);
        settings.forward_all_to = ForwardAllTo::Number;
        settings.forward_all_number = Some("(317) 555-0188".into());
        assert_eq!(
            decide(&settings, &rules, &now),
            Decision::Forward("+13175550188".into())
        );
        // A number abroad cannot be forwarded to: voicemail.
        settings.forward_all_number = Some("+442079460958".into());
        assert_eq!(decide(&settings, &rules, &now), Decision::Voicemail);
        settings.dnd_indefinite = true;
        assert_eq!(decide(&settings, &rules, &now), Decision::Away);
    }

    #[test]
    fn a_ring_rule_rings_outside_working_hours() {
        let now = call(Some("3175550100"), &[]);
        let settings = UserSettings {
            working_hours: Some(evenings()),
            ..UserSettings::default()
        };
        assert_eq!(decide(&settings, &[], &now), Decision::Away);
        let mut vip = rule(RuleAction::Ring);
        vip.callers = Some(vec![RuleCaller {
            number: Some("+13175550100".into()),
            customer_id: None,
        }]);
        assert_eq!(decide(&settings, &[vip.clone()], &now), Decision::Ring);
        // The first rule that matches decides.
        let mut forward = rule(RuleAction::Forward);
        forward.forward_to = Some("+13175550177".into());
        assert_eq!(
            decide(&settings, &[forward.clone(), vip.clone()], &now),
            Decision::Forward("+13175550177".into())
        );
        assert_eq!(decide(&settings, &[vip, forward], &now), Decision::Ring);
    }
}
