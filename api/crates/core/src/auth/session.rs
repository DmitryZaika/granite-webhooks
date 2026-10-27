//! Port of `getUser` + `handlePermissions` from the Remix app's
//! `app/utils/session.server.ts`. Keep the two in sync until Remix is gone.

use crate::billing::FIRST_PAYMENT_DAYS;
use crate::entitlements::{Entitlements, plan_columns};
use crate::openapi::ToSchema;
use serde::Serialize;
use sqlx::{FromRow, MySqlPool};

/// Sessions older than this are rejected even if `expiration_date` is later
/// (Remix `SESSION_MAX_AGE_MONTHS`).
pub const SESSION_MAX_AGE_MONTHS: i32 = 2;

/// `Positions.SuperAdmin` in the Remix app.
pub const SUPER_ADMIN_POSITION: i32 = 9;

/// The signed-in user, after the super-admin rule is applied.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, FromRow, ToSchema)]
pub struct SessionUser {
    /// User id.
    pub id: i32,
    /// Login email.
    pub email: String,
    /// Display name.
    pub name: Option<String>,
    /// Personal phone number.
    pub phone_number: Option<String>,
    /// Staff member of the company (sees customers, deals, inventory).
    pub is_employee: bool,
    /// Company admin. Always true for super admins.
    pub is_admin: bool,
    /// Platform-wide superuser.
    pub is_superuser: bool,
    /// Company every request acts in. For a super admin this is the company
    /// selected in the session, otherwise the user's own company.
    pub company_id: i32,
    /// UI preference: the sidebar is pinned open.
    pub pined_bar: bool,
    /// CloudTalk telephony agent id, when the user has a CloudTalk seat.
    pub cloudtalk_agent_id: Option<String>,
    /// Outbound CloudTalk phone number.
    pub cloudtalk_phone_number: Option<String>,
    /// RingCentral extension id, when the user has a RingCentral seat.
    pub ringcentral_extension_id: Option<String>,
    /// Outbound RingCentral phone number.
    pub ringcentral_phone_number: Option<String>,
    /// Feature flag: the user may use e-signatures.
    pub is_signature_available: bool,
    /// The add-ons the acting company's plan includes.
    #[sqlx(flatten)]
    #[serde(skip)]
    pub plan: Entitlements,
}

#[derive(Debug, Clone, Copy, FromRow)]
pub struct UserPositionRow {
    pub position_id: i32,
    pub company_id: i32,
}

/// Same shape check as Remix `validSessionId`: a UUID string.
pub fn valid_session_id(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() == 36
        && bytes.iter().enumerate().all(|(i, b)| match i {
            8 | 13 | 18 | 23 => *b == b'-',
            _ => b.is_ascii_hexdigit(),
        })
}

/// `company.billing_required` is on and the company's access is paused: its
/// payment is suspended, or it never paid and its first-payment window
/// (`FIRST_PAYMENT_DAYS` from `billing_required_at`) ended. Expects `company`
/// and a LEFT JOINed `company_billing`; binds the window's days.
const BILLING_PAUSED: &str = r"
    COALESCE(company.billing_required, 0) <> 0
    AND (company_billing.payment_status = 'suspended'
         OR (COALESCE(company_billing.payment_status, 'none')
               NOT IN ('active', 'past_due', 'suspended')
             AND company.billing_required_at IS NOT NULL
             AND company.billing_required_at <= UTC_TIMESTAMP() - INTERVAL ? DAY))";

/// A live session's user, and whether their own company's access is paused
/// for billing.
#[derive(Debug, Clone, FromRow)]
pub struct SessionRow {
    #[sqlx(flatten)]
    pub user: SessionUser,
    pub billing_paused: bool,
}

pub async fn find_session(
    pool: &MySqlPool,
    session_id: &str,
) -> Result<Option<SessionRow>, sqlx::Error> {
    if !valid_session_id(session_id) {
        return Ok(None);
    }
    sqlx::query_as::<_, SessionRow>(sqlx::AssertSqlSafe(format!(
        r"
        SELECT users.id, users.email, users.name, users.phone_number,
               COALESCE(users.is_employee, 0) AS is_employee,
               COALESCE(users.is_admin, 0) AS is_admin,
               COALESCE(users.is_superuser, 0) AS is_superuser,
               users.company_id, users.pined_bar, users.cloudtalk_agent_id,
               users.cloudtalk_phone_number, users.ringcentral_extension_id,
               users.ringcentral_phone_number, users.is_signature_available,
               {plan},
               COALESCE({BILLING_PAUSED}, 0) AS billing_paused
        FROM users
        JOIN sessions ON sessions.user_id = users.id
        LEFT JOIN company ON company.id = users.company_id
        LEFT JOIN company_billing ON company_billing.company_id = users.company_id
        WHERE sessions.id = ?
          AND sessions.expiration_date > CURRENT_TIMESTAMP
          AND sessions.created_date > DATE_SUB(CURRENT_TIMESTAMP, INTERVAL ? MONTH)
          AND sessions.is_deleted = 0
          AND users.is_deleted = 0
        ",
        plan = plan_columns("company"),
    )))
    .bind(FIRST_PAYMENT_DAYS)
    .bind(session_id)
    .bind(SESSION_MAX_AGE_MONTHS)
    .fetch_optional(pool)
    .await
}

pub async fn find_session_user(
    pool: &MySqlPool,
    session_id: &str,
) -> Result<Option<SessionUser>, sqlx::Error> {
    Ok(find_session(pool, session_id).await?.map(|row| row.user))
}

/// Whether `company_id`'s access is paused for billing (a super admin acting
/// in another company).
pub async fn company_billing_paused(
    pool: &MySqlPool,
    company_id: i32,
) -> Result<bool, sqlx::Error> {
    let paused: Option<bool> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        r"
        SELECT COALESCE({BILLING_PAUSED}, 0)
        FROM company
        LEFT JOIN company_billing ON company_billing.company_id = company.id
        WHERE company.id = ?
        "
    )))
    .bind(FIRST_PAYMENT_DAYS)
    .bind(company_id)
    .fetch_optional(pool)
    .await?;
    Ok(paused.unwrap_or(false))
}

/// All of a user's position rows, across every company.
pub async fn user_positions(
    pool: &MySqlPool,
    user_id: i32,
) -> Result<Vec<UserPositionRow>, sqlx::Error> {
    sqlx::query_as::<_, UserPositionRow>(
        "SELECT position_id, company_id FROM users_positions WHERE user_id = ?",
    )
    .bind(user_id)
    .fetch_all(pool)
    .await
}

/// Super admins get admin + employee access, and may act inside any company
/// they hold the Super Admin position in (via the session's `activeCompanyId`).
pub fn effective_user(
    user: SessionUser,
    positions: &[UserPositionRow],
    active_company_id: Option<i32>,
) -> SessionUser {
    let super_admin_companies: Vec<i32> = positions
        .iter()
        .filter(|row| row.position_id == SUPER_ADMIN_POSITION)
        .map(|row| row.company_id)
        .collect();
    if super_admin_companies.is_empty() {
        return user;
    }
    let company_id = match active_company_id {
        Some(id) if super_admin_companies.contains(&id) => id,
        _ => user.company_id,
    };
    SessionUser {
        is_admin: true,
        is_employee: true,
        company_id,
        ..user
    }
}

/// A live user of `company_id` by id, as a session would load them.
pub async fn find_user(
    pool: &MySqlPool,
    user_id: i32,
    company_id: i32,
) -> Result<Option<SessionUser>, sqlx::Error> {
    sqlx::query_as::<_, SessionUser>(sqlx::AssertSqlSafe(format!(
        r"
        SELECT users.id, users.email, users.name, users.phone_number,
               COALESCE(users.is_employee, 0) AS is_employee,
               COALESCE(users.is_admin, 0) AS is_admin,
               COALESCE(users.is_superuser, 0) AS is_superuser,
               users.company_id, users.pined_bar, users.cloudtalk_agent_id,
               users.cloudtalk_phone_number, users.ringcentral_extension_id,
               users.ringcentral_phone_number, users.is_signature_available,
               {}
        FROM users
        LEFT JOIN company ON company.id = users.company_id
        WHERE users.id = ? AND users.company_id = ? AND users.is_deleted = 0
        ",
        plan_columns("company")
    )))
    .bind(user_id)
    .bind(company_id)
    .fetch_optional(pool)
    .await
}

/// A live user by id, as a session would load them, in their own company.
pub async fn find_live_user(
    pool: &MySqlPool,
    user_id: i32,
) -> Result<Option<SessionUser>, sqlx::Error> {
    sqlx::query_as::<_, SessionUser>(sqlx::AssertSqlSafe(format!(
        r"
        SELECT users.id, users.email, users.name, users.phone_number,
               COALESCE(users.is_employee, 0) AS is_employee,
               COALESCE(users.is_admin, 0) AS is_admin,
               COALESCE(users.is_superuser, 0) AS is_superuser,
               users.company_id, users.pined_bar, users.cloudtalk_agent_id,
               users.cloudtalk_phone_number, users.ringcentral_extension_id,
               users.ringcentral_phone_number, users.is_signature_available,
               {}
        FROM users
        LEFT JOIN company ON company.id = users.company_id
        WHERE users.id = ? AND users.is_deleted = 0
        ",
        plan_columns("company")
    )))
    .bind(user_id)
    .fetch_optional(pool)
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user() -> SessionUser {
        SessionUser {
            id: 1,
            email: "a@example.com".into(),
            name: None,
            phone_number: None,
            is_employee: false,
            is_admin: false,
            is_superuser: false,
            company_id: 10,
            pined_bar: false,
            cloudtalk_agent_id: None,
            cloudtalk_phone_number: None,
            ringcentral_extension_id: None,
            ringcentral_phone_number: None,
            is_signature_available: false,
            plan: Entitlements::default(),
        }
    }

    #[test]
    fn session_id_must_be_uuid_shaped() {
        assert!(valid_session_id("aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee"));
        assert!(!valid_session_id("aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeee"));
        assert!(!valid_session_id("aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeeg"));
        assert!(!valid_session_id("' OR 1=1 --"));
    }

    #[test]
    fn super_admin_switches_only_into_own_companies() {
        let positions = [
            UserPositionRow {
                position_id: SUPER_ADMIN_POSITION,
                company_id: 20,
            },
            UserPositionRow {
                position_id: 1,
                company_id: 30,
            },
        ];
        let switched = effective_user(user(), &positions, Some(20));
        assert_eq!(switched.company_id, 20);
        assert!(switched.is_admin && switched.is_employee);

        let refused = effective_user(user(), &positions, Some(30));
        assert_eq!(refused.company_id, 10);
        assert!(refused.is_admin);
    }

    #[test]
    fn regular_user_ignores_active_company() {
        let positions = [UserPositionRow {
            position_id: 1,
            company_id: 20,
        }];
        let same = effective_user(user(), &positions, Some(20));
        assert_eq!(same, user());
    }
}
