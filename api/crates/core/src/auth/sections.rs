//! Position access: the tabs of the app each position opens.
//!
//! Each company picks, for every position of [`CONFIGURABLE_POSITIONS`], the
//! tabs it opens (`company_position_sections`; a company without a row uses
//! [`default_tabs`]). Someone holding several of them (in the company the
//! request acts in) gets all their tabs together. Most positions open every
//! main-app tab by default, so nothing changes until a company takes some
//! away; Inventory Manager and Scheduling Manager open only a few. The
//! portal positions (Shop Worker, Installer, Marketing, Walk-in, Template
//! Guy) keep their own apps, and Super Admin, superusers and users without a
//! listed position keep the whole app.
//!
//! The Admin switch still opens the whole admin side: for an admin, position
//! access hides only main-app tabs, and the API keeps all of its data. For
//! someone without it, an admin tab opens that admin page, and the API treats
//! them as an admin on that page's data ([`Access::allows_admin`]).
//!
//! The API refuses only data that just closed tabs use ([`OWNED_API`]); data
//! several pages share (customers, stones, the schedule) stays readable for
//! the pages that are open. The check runs in the extractors
//! (`authenticate_with_access`). The app hides the tabs and their pages from
//! the same keys (`sections` of `GET /v1/app/shell`).

use crate::auth::session::{SUPER_ADMIN_POSITION, SessionUser, UserPositionRow};
use crate::error::ApiError;
use sqlx::MySqlPool;

/// `positions.id` of Inventory Manager.
pub const INVENTORY_MANAGER_POSITION: i32 = 12;
/// `positions.id` of Scheduling Manager.
pub const SCHEDULING_MANAGER_POSITION: i32 = 13;

/// The positions whose tabs a company may change, in the Company page's
/// order: Sales Rep, Sales Manager, Manager, Office Manager, Shop Manager,
/// Inventory Manager, Scheduling Manager.
pub const CONFIGURABLE_POSITIONS: [i32; 7] = [
    1,
    2,
    5,
    10,
    3,
    INVENTORY_MANAGER_POSITION,
    SCHEDULING_MANAGER_POSITION,
];

/// What an admin tab opens as admin: the inventory.
const INVENTORY_API: [&str; 7] = [
    "/v1/stones",
    "/v1/sinks",
    "/v1/faucets",
    "/v1/slabs",
    "/v1/sales",
    "/v1/suppliers",
    "/v1/supplier-files",
];

/// What an admin tab opens as admin: a customer with their deals, quotes,
/// emails, texts and signatures (not the phone line's own settings).
const CRM_API: [&str; 18] = [
    "/v1/customers",
    "/v1/customer-sources",
    "/v1/deals",
    "/v1/deal-groups",
    "/v1/deal-lists",
    "/v1/deal-activities",
    "/v1/deal-pipelines",
    "/v1/deal-stages",
    "/v1/questionnaires",
    "/v1/projects",
    "/v1/quotes",
    "/v1/emails",
    "/v1/email-attachments",
    "/v1/sms",
    "/v1/calls",
    "/v1/call-transcripts",
    "/v1/signature-requests",
    "/v1/signatures",
];

/// What an admin tab opens as admin: the schedule board, its events and
/// calendars.
const SCHEDULE_API: [&str; 3] = ["/v1/schedule", "/v1/events", "/v1/calendars"];

/// One tab a position may open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tab {
    /// Stable key, stored in `company_position_sections` and sent to the app.
    pub key: &'static str,
    /// Its name on the Company page (admin tabs are listed apart).
    pub label: &'static str,
    /// An admin page: it opens for someone without the Admin switch only
    /// when one of their positions has it.
    pub admin: bool,
    /// For an admin tab, the API its page calls, where someone without the
    /// Admin switch is treated as an admin (each path and everything under
    /// it). Empty for main-app tabs.
    pub api: &'static [&'static str],
}

const fn main_tab(key: &'static str, label: &'static str) -> Tab {
    Tab {
        key,
        label,
        admin: false,
        api: &[],
    }
}

const fn admin_tab(key: &'static str, label: &'static str, api: &'static [&'static str]) -> Tab {
    Tab {
        key,
        label,
        admin: true,
        api,
    }
}

/// Every tab, in the Company page's order: the main app, then the admin
/// pages (as their sidebars list them).
pub const TABS: [Tab; 40] = [
    main_tab("dashboard", "Dashboard"),
    main_tab("stones", "Stones"),
    main_tab("sinks", "Sinks"),
    main_tab("faucets", "Faucets"),
    main_tab("customers", "Customers"),
    main_tab("deals", "Deals"),
    main_tab("quotes", "Quotes"),
    main_tab("emails", "Emails"),
    main_tab("sms", "SMS"),
    main_tab("schedule", "Schedule"),
    main_tab("transactions", "Transactions"),
    main_tab("samples", "Samples"),
    main_tab("special_order", "Special Order"),
    main_tab("checklists", "Checklists"),
    main_tab("suppliers", "Suppliers"),
    main_tab("supports", "Supports"),
    main_tab("documents", "Documents"),
    main_tab("images", "Images"),
    main_tab("instructions", "Instructions"),
    main_tab("esignatures", "eSignatures"),
    admin_tab("admin_stones", "Stones", &INVENTORY_API),
    admin_tab("admin_sinks", "Sinks", &INVENTORY_API),
    admin_tab("admin_faucets", "Faucets", &INVENTORY_API),
    admin_tab(
        "admin_suppliers",
        "Suppliers",
        &["/v1/suppliers", "/v1/supplier-files"],
    ),
    admin_tab("admin_supports", "Supports", &["/v1/supports"]),
    admin_tab("admin_documents", "Documents", &["/v1/documents"]),
    admin_tab(
        "admin_images",
        "Images",
        &["/v1/images", "/v1/image-folders", "/v1/image-library"],
    ),
    admin_tab("admin_instructions", "Instructions", &["/v1/instructions"]),
    admin_tab(
        "admin_esignatures",
        "eSignatures",
        &["/v1/signature-requests", "/v1/signatures"],
    ),
    admin_tab("admin_customers", "Customers", &CRM_API),
    admin_tab(
        "admin_statistics",
        "Statistic",
        &["/v1/stats", "/v1/customers", "/v1/qbo/class-sales"],
    ),
    admin_tab("admin_deals", "Deals", &CRM_API),
    admin_tab(
        "admin_automations",
        "Automations",
        &[
            "/v1/email-templates",
            "/v1/sms-templates",
            "/v1/sms-flows",
            "/v1/followups",
            "/v1/templates",
        ],
    ),
    admin_tab(
        "admin_quotes",
        "Quotes",
        &[
            "/v1/quotes",
            "/v1/quote-price-lists",
            "/v1/quote-forms",
            "/v1/quote-extras",
            "/v1/quote-presets",
            "/v1/quote-tax-rates",
            "/v1/quote-payment-terms",
            "/v1/quote-settings",
            "/v1/quote-email-templates",
            "/v1/quote-sale-events",
            "/v1/online-quote-options",
            "/v1/commissions",
        ],
    ),
    admin_tab("admin_emails", "Emails", &["/v1/emails", "/v1/email-attachments"]),
    admin_tab(
        "admin_transactions",
        "Transactions",
        &["/v1/transactions", "/v1/stones", "/v1/slabs", "/v1/sales"],
    ),
    admin_tab("admin_surveys", "Survey", &["/v1/survey-results"]),
    admin_tab(
        "admin_users_activity",
        "Users Activity",
        &[
            "/v1/users/activity",
            "/v1/users/call-activity",
            "/v1/onboarding/team",
        ],
    ),
    admin_tab("admin_schedule", "Schedule", &SCHEDULE_API),
    admin_tab("admin_sms", "SMS", &["/v1/sms"]),
];

/// API only some tabs use, and those tabs: someone limited to tabs is
/// refused it when none of them is open. Everything else stays open, since
/// several pages share it.
const OWNED_API: [(&str, &[&str]); 3] = [
    ("/v1/stats", &["dashboard", "admin_statistics"]),
    (
        "/v1/transactions",
        &[
            "transactions",
            "admin_transactions",
            "schedule",
            "admin_schedule",
        ],
    ),
    ("/v1/checklists", &["checklists"]),
];

/// The tab with this key.
pub fn tab(key: &str) -> Option<&'static Tab> {
    TABS.iter().find(|tab| tab.key == key)
}

/// The tabs a position opens in a company that has not changed them, in
/// [`TABS`] order: every main-app tab, except for Inventory Manager and
/// Scheduling Manager.
pub fn default_tabs(position_id: i32) -> Vec<&'static str> {
    let keys: &[&str] = match position_id {
        INVENTORY_MANAGER_POSITION => &[
            "stones",
            "sinks",
            "faucets",
            "customers",
            "admin_stones",
            "admin_sinks",
            "admin_faucets",
        ],
        SCHEDULING_MANAGER_POSITION => &["customers", "schedule"],
        _ => {
            return TABS
                .iter()
                .filter(|tab| !tab.admin)
                .map(|tab| tab.key)
                .collect();
        }
    };
    TABS.iter()
        .map(|tab| tab.key)
        .filter(|key| keys.contains(key))
        .collect()
}

/// The tabs an older key stood for (before every tab had its own).
fn legacy(key: &str) -> &'static [&'static str] {
    match key {
        "inventory" => &["stones", "sinks", "faucets"],
        "inventory_admin" => &["admin_stones", "admin_sinks", "admin_faucets"],
        "schedule_admin" => &["admin_schedule"],
        "resources" => &[
            "suppliers",
            "supports",
            "documents",
            "images",
            "instructions",
            "esignatures",
        ],
        _ => &[],
    }
}

/// Tab keys from their stored form (comma-separated), older keys expanded,
/// unknown keys dropped, in [`TABS`] order.
pub fn parse_tabs(stored: &str) -> Vec<&'static str> {
    let keys: Vec<&str> = stored.split(',').map(str::trim).collect();
    TABS.iter()
        .map(|tab| tab.key)
        .filter(|key| {
            keys.iter()
                .any(|stored| stored == key || legacy(stored).contains(key))
        })
        .collect()
}

/// What a user may reach in the company the request acts in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Access {
    /// The whole app (whatever their flags and other positions allow).
    Full,
    /// Only these tabs.
    Tabs(Vec<&'static str>),
}

/// `path` is `prefix` or under it (`/v1/stonesx` is not under `/v1/stones`).
fn under(path: &str, prefix: &str) -> bool {
    path.strip_prefix(prefix)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
}

impl Access {
    /// A request to `path` stays inside what they reach: no closed tab
    /// alone uses it, or an admin tab of theirs opens it.
    pub fn allows(&self, path: &str) -> bool {
        match self {
            Self::Full => true,
            Self::Tabs(open) => {
                OWNED_API
                    .iter()
                    .filter(|(prefix, _)| under(path, prefix))
                    .all(|(_, owners)| owners.iter().any(|owner| open.contains(owner)))
                    || self.allows_admin(path)
            }
        }
    }

    /// One of their admin tabs opens `path`: they act there as a company
    /// admin, without the Admin switch.
    pub fn allows_admin(&self, path: &str) -> bool {
        match self {
            Self::Full => false,
            Self::Tabs(open) => open.iter().filter_map(|key| tab(key)).any(|tab| {
                tab.admin && tab.api.iter().any(|prefix| under(path, prefix))
            }),
        }
    }
}

/// The positions of [`CONFIGURABLE_POSITIONS`] `user` holds in the company
/// the request acts in, or none when they keep the whole app (a superuser,
/// or a Super Admin there).
pub fn configured_positions(user: &SessionUser, positions: &[UserPositionRow]) -> Vec<i32> {
    if user.is_superuser {
        return Vec::new();
    }
    let here: Vec<i32> = positions
        .iter()
        .filter(|row| row.company_id == user.company_id)
        .map(|row| row.position_id)
        .collect();
    if here.contains(&SUPER_ADMIN_POSITION) {
        return Vec::new();
    }
    CONFIGURABLE_POSITIONS
        .into_iter()
        .filter(|position| here.contains(position))
        .collect()
}

/// The tabs a company gives each position of [`CONFIGURABLE_POSITIONS`]:
/// its own list where it changed one (third field true), the defaults
/// otherwise.
pub async fn company_tabs(
    pool: &MySqlPool,
    company_id: i32,
) -> Result<Vec<(i32, Vec<&'static str>, bool)>, ApiError> {
    let rows: Vec<(i32, String)> = sqlx::query_as(
        "SELECT position_id, sections FROM company_position_sections WHERE company_id = ?",
    )
    .bind(company_id)
    .fetch_all(pool)
    .await?;
    Ok(CONFIGURABLE_POSITIONS
        .into_iter()
        .map(
            |position| match rows.iter().find(|(id, _)| *id == position) {
                Some((_, stored)) => (position, parse_tabs(stored), true),
                None => (position, default_tabs(position), false),
            },
        )
        .collect())
}

/// The tabs of `user`'s positions together, in [`TABS`] order; `None` when
/// they limit nothing: no listed position, or every main-app tab open and
/// no admin tab that adds to their flags (`sections` of the app shell).
pub async fn user_tabs(
    pool: &MySqlPool,
    user: &SessionUser,
    positions: &[UserPositionRow],
) -> Result<Option<Vec<&'static str>>, ApiError> {
    let held = configured_positions(user, positions);
    if held.is_empty() {
        return Ok(None);
    }
    let company = company_tabs(pool, user.company_id).await?;
    let open: Vec<&'static str> = TABS
        .iter()
        .map(|tab| tab.key)
        .filter(|key| {
            company
                .iter()
                .any(|(position, tabs, _)| held.contains(position) && tabs.contains(key))
        })
        .collect();
    let every_main = TABS
        .iter()
        .filter(|tab| !tab.admin)
        .all(|tab| open.contains(&tab.key));
    let adds_admin = !user.is_admin && open.iter().any(|key| tab(key).is_some_and(|t| t.admin));
    Ok(if every_main && !adds_admin {
        None
    } else {
        Some(open)
    })
}

/// What `user` reaches in the company the request acts in. Admins keep the
/// whole API: their positions hide only main-app tabs.
pub async fn user_access(
    pool: &MySqlPool,
    user: &SessionUser,
    positions: &[UserPositionRow],
) -> Result<Access, ApiError> {
    if user.is_admin || user.is_superuser {
        return Ok(Access::Full);
    }
    Ok(match user_tabs(pool, user, positions).await? {
        Some(open) => Access::Tabs(open),
        None => Access::Full,
    })
}

/// The refusal for a request outside someone's tabs.
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
    fn only_listed_positions_in_the_acting_company_count() {
        let u = user(3);
        assert_eq!(configured_positions(&u, &rows(&[])), Vec::<i32>::new());
        // Installer and Shop Worker keep their own apps.
        assert_eq!(
            configured_positions(&u, &rows(&[(6, 3), (4, 3)])),
            Vec::<i32>::new()
        );
        assert_eq!(
            configured_positions(&u, &rows(&[(12, 4)])),
            Vec::<i32>::new()
        );
        assert_eq!(
            configured_positions(&u, &rows(&[(12, 3), (6, 3)])),
            vec![12]
        );
        assert_eq!(
            configured_positions(&u, &rows(&[(13, 3), (1, 3), (12, 3)])),
            vec![1, 12, 13]
        );
        assert!(configured_positions(&u, &rows(&[(12, 3), (9, 3)])).is_empty());
        let mut superuser = user(3);
        superuser.is_superuser = true;
        assert!(configured_positions(&superuser, &rows(&[(12, 3)])).is_empty());
    }

    #[test]
    fn only_data_of_closed_tabs_alone_is_refused() {
        let access = Access::Tabs(vec!["customers", "schedule"]);
        for path in [
            "/v1/customers/7",
            "/v1/stones",
            "/v1/calendars",
            "/v1/transactions/3",
            "/v1/me",
            "/v1/statsx",
        ] {
            assert!(access.allows(path), "{path}");
        }
        for path in ["/v1/checklists", "/v1/stats/overview"] {
            assert!(!access.allows(path), "{path}");
        }
        let inventory = Access::Tabs(vec!["stones"]);
        assert!(!inventory.allows("/v1/transactions"));
        assert!(Access::Tabs(vec!["admin_transactions"]).allows("/v1/transactions"));
        assert!(Access::Full.allows("/v1/checklists"));
    }

    #[test]
    fn admin_tabs_open_their_api_as_admin() {
        let employee = Access::Tabs(vec!["stones"]);
        assert!(employee.allows("/v1/stones/7"));
        assert!(!employee.allows_admin("/v1/stones/7"));
        let admin = Access::Tabs(vec!["admin_stones"]);
        assert!(admin.allows_admin("/v1/stones/7"));
        assert!(!admin.allows_admin("/v1/customers/7"));
        assert!(!admin.allows_admin("/v1/phone/admin/numbers"));
        let statistics = Access::Tabs(vec!["admin_statistics"]);
        assert!(statistics.allows("/v1/stats/overview"));
        assert!(!Access::Full.allows_admin("/v1/stones"));
    }

    #[test]
    fn stored_tabs_expand_old_keys_drop_unknown_ones_and_keep_order() {
        assert_eq!(
            parse_tabs("schedule, customers,nope"),
            vec!["customers", "schedule"]
        );
        assert_eq!(
            parse_tabs("inventory_admin,inventory,schedule_admin"),
            vec![
                "stones",
                "sinks",
                "faucets",
                "admin_stones",
                "admin_sinks",
                "admin_faucets",
                "admin_schedule"
            ]
        );
        assert_eq!(parse_tabs(""), Vec::<&str>::new());
    }

    #[test]
    fn defaults_are_known_tabs_and_most_positions_open_the_whole_main_app() {
        for position in CONFIGURABLE_POSITIONS {
            for key in default_tabs(position) {
                assert!(tab(key).is_some(), "{key}");
            }
        }
        let main: Vec<&str> = TABS
            .iter()
            .filter(|tab| !tab.admin)
            .map(|tab| tab.key)
            .collect();
        for position in [1, 2, 3, 5, 10] {
            assert_eq!(default_tabs(position), main, "{position}");
        }
        assert_eq!(default_tabs(13), vec!["customers", "schedule"]);
        assert!(default_tabs(12).contains(&"admin_stones"));
        assert!(!default_tabs(12).contains(&"transactions"));
    }

    #[test]
    fn keys_are_unique() {
        for (i, a) in TABS.iter().enumerate() {
            assert!(TABS[i + 1..].iter().all(|b| b.key != a.key), "{}", a.key);
        }
    }
}
