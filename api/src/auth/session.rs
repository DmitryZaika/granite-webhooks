//! Port of `getUser` + `handlePermissions` from the Remix app's
//! `app/utils/session.server.ts`. Keep the two in sync until Remix is gone.

use serde::Serialize;
use sqlx::{FromRow, MySqlPool};
use utoipa::ToSchema;

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

pub async fn find_session_user(
    pool: &MySqlPool,
    session_id: &str,
) -> Result<Option<SessionUser>, sqlx::Error> {
    if !valid_session_id(session_id) {
        return Ok(None);
    }
    sqlx::query_as::<_, SessionUser>(
        r"
        SELECT users.id, users.email, users.name, users.phone_number,
               COALESCE(users.is_employee, 0) AS is_employee,
               COALESCE(users.is_admin, 0) AS is_admin,
               COALESCE(users.is_superuser, 0) AS is_superuser,
               users.company_id, users.pined_bar, users.cloudtalk_agent_id,
               users.cloudtalk_phone_number, users.ringcentral_extension_id,
               users.ringcentral_phone_number, users.is_signature_available
        FROM users
        JOIN sessions ON sessions.user_id = users.id
        WHERE sessions.id = ?
          AND sessions.expiration_date > CURRENT_TIMESTAMP
          AND sessions.created_date > DATE_SUB(CURRENT_TIMESTAMP, INTERVAL ? MONTH)
          AND sessions.is_deleted = 0
          AND users.is_deleted = 0
        ",
    )
    .bind(session_id)
    .bind(SESSION_MAX_AGE_MONTHS)
    .fetch_optional(pool)
    .await
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
