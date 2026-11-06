//! Position skills: what a company lets each position do, beside the tabs it
//! opens (Position access on the Company page).
//!
//! A tab opens a page; a skill lets its people change things there. Without
//! [`EDIT_QUOTES`] quotes stay readable; without [`EDIT_TRANSACTIONS`] a
//! transaction stays readable. Each company picks the skills of every
//! position of [`CONFIGURABLE_POSITIONS`] (`company_position_sections.skills`;
//! NULL, or a company without a row, uses [`default_skills`]). Someone
//! holding several of them has the skills of all of them. Admins, superusers,
//! Super Admins and people without such a position keep every skill.
//!
//! The API refuses a skill's routes ([`required`]) in the extractors
//! (`authenticate_with_access`); the app hides their buttons from the same
//! keys (`skills` of `GET /v1/app/shell`).

use crate::auth::sections::{
    CONFIGURABLE_POSITIONS, SCHEDULING_MANAGER_POSITION, configured_positions,
};
use crate::auth::session::{SessionUser, UserPositionRow};
use crate::error::ApiError;
use axum::http::Method;
use sqlx::MySqlPool;

/// Sell slabs from Stones (and the sale form after a won deal), edit or
/// cancel a sale.
pub const SELL_SLABS: &str = "sell_slabs";
/// Change a transaction: its sale, slabs (cut, remove, replace, square feet),
/// installed date, or cancel it.
pub const EDIT_TRANSACTIONS: &str = "edit_transactions";
/// Make, change, copy, sell and delete quotes.
pub const EDIT_QUOTES: &str = "edit_quotes";
/// The schedule's Auto plan, a day's route (reroute) and an event's Send SMS.
pub const PLAN_SCHEDULE: &str = "plan_schedule";

/// One skill a position may have.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Skill {
    /// Stable key, stored in `company_position_sections.skills` and sent to
    /// the app.
    pub key: &'static str,
    /// Its name on the Company page.
    pub label: &'static str,
    /// What it lets people do.
    pub description: &'static str,
}

/// Every skill, in the Company page's order.
pub const SKILLS: [Skill; 4] = [
    Skill {
        key: SELL_SLABS,
        label: "Sell slabs",
        description: "Sell slabs from Stones, edit or cancel a sale.",
    },
    Skill {
        key: EDIT_TRANSACTIONS,
        label: "Work with transactions",
        description: "Edit a transaction, cut or swap its slabs, mark it installed, delete it.",
    },
    Skill {
        key: EDIT_QUOTES,
        label: "Edit quotes",
        description: "Make, change, copy, sell and delete quotes. Without it, quotes are read-only.",
    },
    Skill {
        key: PLAN_SCHEDULE,
        label: "Plan the schedule",
        description: "Auto plan, build a day's route and send SMS from an event.",
    },
];

/// The skill with this key.
pub fn skill(key: &str) -> Option<&'static Skill> {
    SKILLS.iter().find(|skill| skill.key == key)
}

/// A position's skills in a company that has not changed them.
///
/// In [`SKILLS`] order: every position sells slabs and works with
/// transactions and quotes, as before skills; only Scheduling Manager plans
/// the schedule.
pub fn default_skills(position_id: i32) -> Vec<&'static str> {
    SKILLS
        .iter()
        .map(|skill| skill.key)
        .filter(|key| *key != PLAN_SCHEDULE || position_id == SCHEDULING_MANAGER_POSITION)
        .collect()
}

/// Skill keys from their stored form (comma-separated), unknown keys
/// dropped, in [`SKILLS`] order.
pub fn parse_skills(stored: &str) -> Vec<&'static str> {
    let keys: Vec<&str> = stored.split(',').map(str::trim).collect();
    SKILLS
        .iter()
        .map(|skill| skill.key)
        .filter(|key| keys.contains(key))
        .collect()
}

/// The skills a company gives each position of [`CONFIGURABLE_POSITIONS`].
///
/// Its own list where it changed one (third field true), the defaults
/// otherwise. Best effort: before the `skills` column exists every position
/// has its defaults.
pub async fn company_skills(
    pool: &MySqlPool,
    company_id: i32,
) -> Vec<(i32, Vec<&'static str>, bool)> {
    let rows: Vec<(i32, String)> = sqlx::query_as(
        "SELECT position_id, skills FROM company_position_sections
          WHERE company_id = ? AND skills IS NOT NULL",
    )
    .bind(company_id)
    .fetch_all(pool)
    .await
    .unwrap_or_default();
    CONFIGURABLE_POSITIONS
        .into_iter()
        .map(
            |position| match rows.iter().find(|(id, _)| *id == position) {
                Some((_, stored)) => (position, parse_skills(stored), true),
                None => (position, default_skills(position), false),
            },
        )
        .collect()
}

/// The skills of `user`'s positions together, in [`SKILLS`] order; `None`
/// when they have every skill (an admin, a superuser, a Super Admin, no
/// listed position, or positions that give them all).
pub async fn user_skills(
    pool: &MySqlPool,
    user: &SessionUser,
    positions: &[UserPositionRow],
) -> Option<Vec<&'static str>> {
    if user.is_admin || user.is_superuser {
        return None;
    }
    let held = configured_positions(user, positions);
    if held.is_empty() {
        return None;
    }
    let company = company_skills(pool, user.company_id).await;
    let have: Vec<&'static str> = SKILLS
        .iter()
        .map(|skill| skill.key)
        .filter(|key| {
            company
                .iter()
                .any(|(position, skills, _)| held.contains(position) && skills.contains(key))
        })
        .collect();
    (have.len() < SKILLS.len()).then_some(have)
}

fn is_id(segment: &str) -> bool {
    !segment.is_empty() && segment.bytes().all(|b| b.is_ascii_digit())
}

/// The skills, any one of which lets `method` on `path` through; `None` when
/// the request needs none. Reading never needs one.
pub fn required(method: &Method, path: &str) -> Option<&'static [&'static str]> {
    if [Method::GET, Method::HEAD, Method::OPTIONS].contains(method) {
        return None;
    }
    let segments: Vec<&str> = path.trim_end_matches('/').split('/').skip(1).collect();
    match segments.as_slice() {
        // A new sale (Stones' Sell, the sale form after a won deal).
        ["v1", "sales"] => Some(&[SELL_SLABS]),
        // Edit or cancel a sale: from Stones or from its transaction.
        ["v1", "sales", id] | ["v1", "sales", id, "unsell"] if is_id(id) => {
            Some(&[SELL_SLABS, EDIT_TRANSACTIONS])
        }
        ["v1", "transactions", id, ..] if is_id(id) => Some(&[EDIT_TRANSACTIONS]),
        ["v1", "quotes"] => Some(&[EDIT_QUOTES]),
        // A quote and what changes it; printing, emailing, the customer's
        // link and QuickBooks invoices only read it.
        ["v1", "quotes", id]
        | [
            "v1",
            "quotes",
            id,
            "autosave" | "drawing" | "copies" | "prices" | "sale",
        ] if is_id(id) => Some(&[EDIT_QUOTES]),
        [
            "v1",
            "schedule",
            "auto-plan" | "optimize-route" | "route-preview",
        ] => Some(&[PLAN_SCHEDULE]),
        ["v1", "events", id, "confirmation"] if is_id(id) => Some(&[PLAN_SCHEDULE]),
        _ => None,
    }
}

/// Refuses `method` on `path` when it needs a skill none of `user`'s
/// positions has. `user.is_admin` must be their own flag, not the one an
/// admin tab lends them.
pub async fn check(
    pool: &MySqlPool,
    user: &SessionUser,
    positions: &[UserPositionRow],
    method: &Method,
    path: &str,
) -> Result<(), ApiError> {
    let Some(needed) = required(method, path) else {
        return Ok(());
    };
    match user_skills(pool, user, positions).await {
        Some(have) if !needed.iter().any(|key| have.contains(key)) => Err(missing(needed[0])),
        _ => Ok(()),
    }
}

/// The refusal for a request that needs a skill someone lacks.
pub fn missing(key: &str) -> ApiError {
    let label = skill(key).map_or(key, |skill| skill.label);
    ApiError::Denied(format!(
        "Your position doesn't have the \"{label}\" skill. A company Manager can add it on the Company page, under Position access."
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_need_their_skill_and_reads_need_none() {
        let post = Method::POST;
        assert_eq!(required(&post, "/v1/sales"), Some(&[SELL_SLABS][..]));
        assert_eq!(
            required(&Method::PUT, "/v1/sales/12"),
            Some(&[SELL_SLABS, EDIT_TRANSACTIONS][..])
        );
        assert_eq!(
            required(&post, "/v1/sales/12/unsell"),
            Some(&[SELL_SLABS, EDIT_TRANSACTIONS][..])
        );
        assert_eq!(
            required(&post, "/v1/transactions/4/slabs/9/cut"),
            Some(&[EDIT_TRANSACTIONS][..])
        );
        assert_eq!(
            required(&Method::PUT, "/v1/transactions/4/slabs/9/square-feet"),
            Some(&[EDIT_TRANSACTIONS][..])
        );
        assert_eq!(required(&post, "/v1/quotes"), Some(&[EDIT_QUOTES][..]));
        for path in [
            "/v1/quotes/3",
            "/v1/quotes/3/autosave",
            "/v1/quotes/3/drawing",
            "/v1/quotes/3/copies",
            "/v1/quotes/3/prices",
            "/v1/quotes/3/sale",
        ] {
            assert_eq!(
                required(&Method::PATCH, path),
                Some(&[EDIT_QUOTES][..]),
                "{path}"
            );
        }
        assert_eq!(
            required(&Method::DELETE, "/v1/quotes/3"),
            Some(&[EDIT_QUOTES][..])
        );
        for path in [
            "/v1/schedule/auto-plan",
            "/v1/schedule/optimize-route",
            "/v1/schedule/route-preview",
            "/v1/events/8/confirmation",
        ] {
            assert_eq!(required(&post, path), Some(&[PLAN_SCHEDULE][..]), "{path}");
        }
        for (method, path) in [
            (Method::GET, "/v1/sales/12"),
            (Method::GET, "/v1/quotes/3"),
            (Method::GET, "/v1/transactions/4"),
            (Method::POST, "/v1/quotes/print-previews"),
            (Method::POST, "/v1/quotes/3/prints"),
            (Method::POST, "/v1/quotes/3/emails"),
            (Method::POST, "/v1/quotes/3/portal-token"),
            (Method::POST, "/v1/quotes/3/invoice-draft"),
            (Method::POST, "/v1/events"),
            (Method::PUT, "/v1/events/8"),
            (Method::DELETE, "/v1/events/8"),
            (Method::POST, "/v1/schedule/notifications/flows"),
            (Method::POST, "/v1/sales-x"),
            (Method::POST, "/v1/quote-forms"),
        ] {
            assert_eq!(required(&method, path), None, "{method} {path}");
        }
    }

    #[test]
    fn defaults_keep_what_each_position_did_before_skills() {
        for position in [1, 2, 3, 5, 10, 12] {
            assert_eq!(
                default_skills(position),
                vec![SELL_SLABS, EDIT_TRANSACTIONS, EDIT_QUOTES],
                "{position}"
            );
        }
        assert_eq!(
            default_skills(13),
            SKILLS.iter().map(|skill| skill.key).collect::<Vec<_>>()
        );
    }

    #[test]
    fn stored_skills_drop_unknown_keys_and_keep_order() {
        assert_eq!(
            parse_skills("plan_schedule, sell_slabs,nope"),
            vec![SELL_SLABS, PLAN_SCHEDULE]
        );
        assert_eq!(parse_skills(""), Vec::<&str>::new());
    }

    #[test]
    fn keys_are_unique_and_never_a_tab() {
        for (i, a) in SKILLS.iter().enumerate() {
            assert!(SKILLS[i + 1..].iter().all(|b| b.key != a.key), "{}", a.key);
            assert!(crate::auth::sections::tab(a.key).is_none(), "{}", a.key);
        }
    }
}
