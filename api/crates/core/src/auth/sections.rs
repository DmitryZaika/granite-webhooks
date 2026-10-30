//! Position sections: the parts of the app a sectioned position opens.
//!
//! Inventory Manager and Scheduling Manager are sectioned: someone who holds
//! only those (in the company the request acts in) reaches only the sections
//! they open, plus [`COMMON_API`]. Each company may change a position's
//! sections (`company_position_sections`); a company without a row uses
//! [`default_sections`]. Holding a [`FULL_ACCESS_POSITIONS`] position beside
//! them, being a superuser, or holding no sectioned position keeps the whole
//! app, as before.
//!
//! The check runs in the extractors (`authenticate_with_access`), so a page's
//! tab and its API are closed together. The app hides the tabs from the same
//! keys (`sections` of `GET /v1/app/shell`).

use crate::auth::session::{SUPER_ADMIN_POSITION, SessionUser, UserPositionRow};
use crate::error::ApiError;
use sqlx::MySqlPool;

/// `positions.id` of Inventory Manager.
pub const INVENTORY_MANAGER_POSITION: i32 = 12;
/// `positions.id` of Scheduling Manager.
pub const SCHEDULING_MANAGER_POSITION: i32 = 13;
/// The positions whose access is a list of sections.
pub const SECTIONED_POSITIONS: [i32; 2] = [INVENTORY_MANAGER_POSITION, SCHEDULING_MANAGER_POSITION];

/// Positions that keep the whole app even beside a sectioned one: Sales
/// Rep, Sales Manager, Manager and Super Admin.
const FULL_ACCESS_POSITIONS: [i32; 4] = [1, 2, 5, SUPER_ADMIN_POSITION];

/// What every signed-in user reaches, whatever their sections: signing in,
/// the app frame, My Account and their own lists, notifications, uploads,
/// the tours, Report a Bug, the AI chat and the shared pick lists.
const COMMON_API: [&str; 19] = [
    "/v1/auth",
    "/v1/app",
    "/v1/me",
    "/v1/public",
    "/v1/notifications",
    "/v1/uploads",
    "/v1/onboarding",
    "/v1/bug-reports",
    "/v1/points-leaderboard",
    "/v1/colors",
    "/v1/stone-types",
    "/v1/sink-types",
    "/v1/places",
    "/v1/ai",
    "/v1/billing/gate",
    "/v1/company/features",
    "/v1/company/tax-rate",
    "/v1/company/drive-miles",
    "/v1/users/sales-reps",
];

/// The CRM: a customer with everything inside it (deals, quotes, emails,
/// texts, calls, files, signatures, invoices), and the pick lists its forms
/// use. Customers, Deals, Quotes, Emails and SMS all open it.
const CRM_API: [&str; 37] = [
    "/v1/customers",
    "/v1/customer-sources",
    "/v1/users/employees",
    "/v1/deals",
    "/v1/deal-groups",
    "/v1/deal-lists",
    "/v1/deal-activities",
    "/v1/deal-pipelines",
    "/v1/deal-stages",
    "/v1/questionnaires",
    "/v1/projects",
    "/v1/quotes",
    "/v1/quote-price-lists",
    "/v1/quote-forms",
    "/v1/quote-extras",
    "/v1/quote-tax-rates",
    "/v1/quote-payment-terms",
    "/v1/quote-settings",
    "/v1/quote-email-templates",
    "/v1/online-quote-options",
    "/v1/emails",
    "/v1/email-attachments",
    "/v1/email-templates",
    "/v1/templates",
    "/v1/sms",
    "/v1/sms-templates",
    "/v1/sms-flows",
    "/v1/calls",
    "/v1/call-transcripts",
    "/v1/phone",
    "/v1/followups",
    "/v1/delayed",
    "/v1/documents",
    "/v1/images",
    "/v1/image-library",
    "/v1/signature-requests",
    "/v1/signatures",
];

/// Stones, sinks and faucets, their slabs, and selling a slab.
const INVENTORY_API: [&str; 7] = [
    "/v1/stones",
    "/v1/sinks",
    "/v1/faucets",
    "/v1/slabs",
    "/v1/sales",
    "/v1/suppliers",
    "/v1/supplier-files",
];

/// The schedule board, its events and calendars.
const SCHEDULE_API: [&str; 3] = ["/v1/schedule", "/v1/events", "/v1/calendars"];

/// One part of the app a sectioned position may open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Section {
    /// Stable key, stored in `company_position_sections` and sent to the app.
    pub key: &'static str,
    /// What the Company page calls it.
    pub label: &'static str,
    /// Opens admin pages: [`crate::auth::AdminUser`] passes on its API for
    /// someone who is not a company admin.
    pub admin: bool,
    /// The API prefixes its pages call (each path and everything under it).
    pub api: &'static [&'static str],
}

/// Every section, in the Company page's order.
pub const SECTIONS: [Section; 15] = [
    Section {
        key: "dashboard",
        label: "Dashboard",
        admin: false,
        api: &["/v1/stats"],
    },
    Section {
        key: "inventory",
        label: "Inventory",
        admin: false,
        api: &INVENTORY_API,
    },
    Section {
        key: "inventory_admin",
        label: "Inventory (admin)",
        admin: true,
        api: &INVENTORY_API,
    },
    Section {
        key: "customers",
        label: "Customers",
        admin: false,
        api: &CRM_API,
    },
    Section {
        key: "deals",
        label: "Deals",
        admin: false,
        api: &CRM_API,
    },
    Section {
        key: "quotes",
        label: "Quotes",
        admin: false,
        api: &CRM_API,
    },
    Section {
        key: "emails",
        label: "Emails",
        admin: false,
        api: &CRM_API,
    },
    Section {
        key: "sms",
        label: "SMS",
        admin: false,
        api: &CRM_API,
    },
    Section {
        key: "schedule",
        label: "Schedule",
        admin: false,
        api: &SCHEDULE_API,
    },
    Section {
        key: "schedule_admin",
        label: "Schedule (admin)",
        admin: true,
        api: &SCHEDULE_API,
    },
    Section {
        key: "transactions",
        label: "Transactions",
        admin: false,
        api: &["/v1/transactions"],
    },
    Section {
        key: "samples",
        label: "Samples",
        admin: false,
        api: &["/v1/stones"],
    },
    Section {
        key: "special_order",
        label: "Special Order",
        admin: false,
        api: &[],
    },
    Section {
        key: "checklists",
        label: "Checklists",
        admin: false,
        api: &["/v1/checklists"],
    },
    Section {
        key: "resources",
        label: "Suppliers, Supports, Documents, Images, Instructions",
        admin: false,
        api: &[
            "/v1/suppliers",
            "/v1/supplier-files",
            "/v1/supports",
            "/v1/documents",
            "/v1/images",
            "/v1/image-folders",
            "/v1/image-library",
            "/v1/instructions",
            "/v1/signature-requests",
            "/v1/signatures",
        ],
    },
];

/// The section with this key.
pub fn section(key: &str) -> Option<&'static Section> {
    SECTIONS.iter().find(|section| section.key == key)
}

/// The sections a position opens in a company that has not changed them.
pub const fn default_sections(position_id: i32) -> &'static [&'static str] {
    match position_id {
        INVENTORY_MANAGER_POSITION => &[
            "inventory",
            "inventory_admin",
            "customers",
            "transactions",
            "samples",
            "special_order",
        ],
        SCHEDULING_MANAGER_POSITION => &["customers", "schedule"],
        _ => &[],
    }
}

/// Section keys from their stored form (comma-separated), unknown keys
/// dropped, in [`SECTIONS`] order.
pub fn parse_sections(stored: &str) -> Vec<&'static str> {
    let keys: Vec<&str> = stored.split(',').map(str::trim).collect();
    SECTIONS
        .iter()
        .filter(|section| keys.contains(&section.key))
        .map(|section| section.key)
        .collect()
}

/// What a user may reach in the company the request acts in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Access {
    /// The whole app (whatever their flags and other positions allow).
    Full,
    /// Only these sections, plus [`COMMON_API`].
    Sections(Vec<&'static str>),
}

/// `path` is `prefix` or under it (`/v1/stonesx` is not under `/v1/stones`).
fn under(path: &str, prefix: &str) -> bool {
    path.strip_prefix(prefix)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
}

impl Access {
    /// A request to `path` stays inside what they reach.
    pub fn allows(&self, path: &str) -> bool {
        match self {
            Self::Full => true,
            Self::Sections(keys) => {
                COMMON_API.iter().any(|prefix| under(path, prefix))
                    || keys
                        .iter()
                        .filter_map(|key| section(key))
                        .any(|section| section.api.iter().any(|prefix| under(path, prefix)))
            }
        }
    }

    /// One of their admin sections covers `path`: admin routes there pass
    /// without the company admin flag.
    pub fn allows_admin(&self, path: &str) -> bool {
        match self {
            Self::Full => false,
            Self::Sections(keys) => keys.iter().filter_map(|key| section(key)).any(|section| {
                section.admin && section.api.iter().any(|prefix| under(path, prefix))
            }),
        }
    }

    /// Their sections, `None` for the whole app (`sections` of the shell).
    pub fn keys(&self) -> Option<Vec<String>> {
        match self {
            Self::Full => None,
            Self::Sections(keys) => Some(keys.iter().map(ToString::to_string).collect()),
        }
    }
}

/// The sectioned positions `user` holds in the company the request acts in,
/// or none when they keep the whole app.
pub fn sectioned_positions(user: &SessionUser, positions: &[UserPositionRow]) -> Vec<i32> {
    if user.is_superuser {
        return Vec::new();
    }
    let here: Vec<i32> = positions
        .iter()
        .filter(|row| row.company_id == user.company_id)
        .map(|row| row.position_id)
        .collect();
    if here
        .iter()
        .any(|position| FULL_ACCESS_POSITIONS.contains(position))
    {
        return Vec::new();
    }
    SECTIONED_POSITIONS
        .into_iter()
        .filter(|position| here.contains(position))
        .collect()
}

/// The sections a company gives each sectioned position: its own list where
/// it changed one, the defaults otherwise. One entry per position of
/// [`SECTIONED_POSITIONS`].
pub async fn company_sections(
    pool: &MySqlPool,
    company_id: i32,
) -> Result<Vec<(i32, Vec<&'static str>, bool)>, ApiError> {
    let rows: Vec<(i32, String)> = sqlx::query_as(
        "SELECT position_id, sections FROM company_position_sections WHERE company_id = ?",
    )
    .bind(company_id)
    .fetch_all(pool)
    .await?;
    Ok(SECTIONED_POSITIONS
        .into_iter()
        .map(
            |position| match rows.iter().find(|(id, _)| *id == position) {
                Some((_, stored)) => (position, parse_sections(stored), true),
                None => (position, default_sections(position).to_vec(), false),
            },
        )
        .collect())
}

/// What `user` reaches in the company the request acts in: the union of
/// their sectioned positions' sections there.
pub async fn user_access(
    pool: &MySqlPool,
    user: &SessionUser,
    positions: &[UserPositionRow],
) -> Result<Access, ApiError> {
    let held = sectioned_positions(user, positions);
    if held.is_empty() {
        return Ok(Access::Full);
    }
    let company = company_sections(pool, user.company_id).await?;
    let keys = SECTIONS
        .iter()
        .map(|section| section.key)
        .filter(|key| {
            company
                .iter()
                .any(|(position, sections, _)| held.contains(position) && sections.contains(key))
        })
        .collect();
    Ok(Access::Sections(keys))
}

/// The refusal for a request outside someone's sections.
pub fn outside_sections() -> ApiError {
    ApiError::Denied(
        "Your position doesn't include this part of the app. A company Manager can add it on the Company page, under Position access.".into(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user(company_id: i32) -> SessionUser {
        SessionUser {
            id: 1,
            email: "a@b.c".into(),
            name: None,
            phone_number: None,
            is_employee: true,
            is_admin: false,
            is_superuser: false,
            company_id,
            pined_bar: false,
            cloudtalk_agent_id: None,
            cloudtalk_phone_number: None,
            ringcentral_extension_id: None,
            ringcentral_phone_number: None,
            is_signature_available: false,
        }
    }

    fn rows(positions: &[(i32, i32)]) -> Vec<UserPositionRow> {
        positions
            .iter()
            .map(|&(position_id, company_id)| UserPositionRow {
                position_id,
                company_id,
            })
            .collect()
    }

    #[test]
    fn only_a_sectioned_position_in_the_acting_company_restricts() {
        let u = user(3);
        assert_eq!(sectioned_positions(&u, &rows(&[])), Vec::<i32>::new());
        assert_eq!(
            sectioned_positions(&u, &rows(&[(10, 3)])),
            Vec::<i32>::new()
        );
        assert_eq!(
            sectioned_positions(&u, &rows(&[(12, 4)])),
            Vec::<i32>::new()
        );
        assert_eq!(
            sectioned_positions(&u, &rows(&[(12, 3), (10, 3)])),
            vec![12]
        );
        assert_eq!(
            sectioned_positions(&u, &rows(&[(13, 3), (12, 3)])),
            vec![12, 13]
        );
        for full in [1, 2, 5, 9] {
            assert!(
                sectioned_positions(&u, &rows(&[(12, 3), (full, 3)])).is_empty(),
                "{full}"
            );
        }
        let mut superuser = user(3);
        superuser.is_superuser = true;
        assert_eq!(
            sectioned_positions(&superuser, &rows(&[(12, 3)])),
            Vec::<i32>::new()
        );
    }

    #[test]
    fn sections_open_their_api_and_the_common_one() {
        let access = Access::Sections(vec!["schedule"]);
        for path in [
            "/v1/schedule",
            "/v1/events/7",
            "/v1/calendars",
            "/v1/me",
            "/v1/app/shell",
        ] {
            assert!(access.allows(path), "{path}");
        }
        for path in [
            "/v1/stones",
            "/v1/customers/7",
            "/v1/admin/users",
            "/v1/schedulex",
            "/v1/users/activity",
        ] {
            assert!(!access.allows(path), "{path}");
        }
        assert!(Access::Full.allows("/v1/admin/users"));
    }

    #[test]
    fn only_admin_sections_pass_admin_routes() {
        let employee = Access::Sections(vec!["inventory"]);
        assert!(employee.allows("/v1/stones/7"));
        assert!(!employee.allows_admin("/v1/stones/7"));
        let admin = Access::Sections(vec!["inventory_admin"]);
        assert!(admin.allows_admin("/v1/stones/7"));
        assert!(!admin.allows_admin("/v1/customers/7"));
        assert!(!Access::Full.allows_admin("/v1/stones"));
    }

    #[test]
    fn stored_sections_drop_unknown_keys_and_keep_order() {
        assert_eq!(
            parse_sections("schedule, customers,nope"),
            vec!["customers", "schedule"]
        );
        assert_eq!(parse_sections(""), Vec::<&str>::new());
    }

    #[test]
    fn defaults_are_known_sections() {
        for position in SECTIONED_POSITIONS {
            for key in default_sections(position) {
                assert!(section(key).is_some(), "{key}");
            }
        }
    }
}
