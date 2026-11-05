//! Who sees which projects, what they may do, and the stage rules.
//!
//! A project's stage is stored as the last manual stage; Installed and Paid &
//! Completed come from its transaction (`sales.installed_date`,
//! `sales.paid_date`), so marking a transaction installed on the Transactions
//! page moves the project with no extra write.

use crate::auth::session::{SessionUser, UserPositionRow, user_positions};
use crate::error::{ApiError, ApiResult};
use crate::openapi::ToSchema;
use serde::{Deserialize, Serialize};
use sqlx::MySqlPool;

const SALES_REP: i32 = 1;
const SALES_MANAGER: i32 = 2;
const SHOP_MANAGER: i32 = 3;
const MANAGER: i32 = 5;
/// Office Manager.
const OFFICE_MANAGER: i32 = 10;
const INVENTORY_MANAGER: i32 = 12;
const SCHEDULING_MANAGER: i32 = 13;

/// Where a project is: the board's four lists, and Paid & Completed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum Stage {
    /// Awaiting Template.
    New,
    /// Templated.
    Templated,
    /// In Fabrication.
    Fabrication,
    /// Installed (its transaction has an installed date, or the installer
    /// tapped Completed).
    Installed,
    /// Paid & Completed (installed and paid).
    Completed,
}

impl Stage {
    pub const ALL: [Self; 5] = [
        Self::New,
        Self::Templated,
        Self::Fabrication,
        Self::Installed,
        Self::Completed,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::New => "new",
            Self::Templated => "templated",
            Self::Fabrication => "fabrication",
            Self::Installed => "installed",
            Self::Completed => "completed",
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::New => "Awaiting Template",
            Self::Templated => "Templated",
            Self::Fabrication => "In Fabrication",
            Self::Installed => "Installed",
            Self::Completed => "Paid & Completed",
        }
    }
}

impl TryFrom<String> for Stage {
    type Error = String;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::ALL
            .into_iter()
            .find(|stage| stage.as_str() == value)
            .ok_or_else(|| format!("unknown project stage {value}"))
    }
}

/// The project's Templates and Installations jobs (`events e`, `calendars
/// cal`): linked to it, or of its customer, made since it started and
/// linked to none.
pub const PROJECT_JOB_SQL: &str = "e.deleted_date IS NULL
      AND (e.project_id = p.id
           OR (e.project_id IS NULL AND e.customer_id = p.customer_id
               AND e.created_date >= p.created_at - INTERVAL 1 DAY))";

/// The stage of `projects p` with its transaction `sales s` (LEFT JOIN).
///
/// Projects move on their own: the template maker's Completed makes it
/// Templated, the shop's Fabricated In Fabrication, the installer's
/// Completed Installed, and the transaction's installed and paid dates
/// Installed and Paid & Completed. Paid is the transaction's paid date or the
/// customer's QuickBooks invoices all paid (`p.qbo_paid_at`, see `payments`).
/// A template or installation on the calendar does not move it: the card
/// shows it as its status (`template_date`, `install_date`). A stage someone
/// moved it to by hand counts when it is further on (an older stored
/// Template/Install Scheduled counts as Awaiting Template/In Fabrication).
pub const STAGE_SQL: &str = "(CASE WHEN s.installed_date IS NOT NULL
        THEN IF(s.paid_date IS NOT NULL OR p.qbo_paid_at IS NOT NULL, 'completed', 'installed')
      ELSE ELT(GREATEST(
          FIELD(CASE WHEN p.stage_code IN ('installed', 'completed', 'install_scheduled')
                       THEN 'fabrication'
                     WHEN p.stage_code = 'template_scheduled' THEN 'new'
                     ELSE CAST(p.stage_code AS CHAR) END,
                'new', 'templated', 'fabrication'),
          (SELECT COALESCE(MAX(CASE
                    WHEN cal.slug = 'installations'
                      THEN IF(s.paid_date IS NOT NULL OR p.qbo_paid_at IS NOT NULL, 5, 4)
                    ELSE 2 END), 1)
             FROM events e
             JOIN calendars cal ON cal.id = e.calendar_id
                               AND cal.slug IN ('templates', 'installations')
            WHERE e.crew_status = 'completed'
              AND e.deleted_date IS NULL
              AND (e.project_id = p.id
                   OR (e.project_id IS NULL AND e.customer_id = p.customer_id
                       AND e.created_date >= p.created_at - INTERVAL 1 DAY))),
          IF(p.fabricated_at IS NOT NULL, 3, 1)),
        'new', 'templated', 'fabrication', 'installed', 'completed') END)";

/// When the project's next `slug` job (`templates` or `installations`) is on
/// the calendar, UTC ISO; NULL when none waits (done ones don't count).
pub fn job_date_sql(slug: &str) -> String {
    format!(
        "(SELECT DATE_FORMAT(MIN(e.start_date), '%Y-%m-%dT%H:%i:%sZ')
            FROM events e
            JOIN calendars cal ON cal.id = e.calendar_id AND cal.slug = '{slug}'
           WHERE COALESCE(e.unscheduled, 0) = 0 AND COALESCE(e.crew_status, '') <> 'completed'
             AND {PROJECT_JOB_SQL})"
    )
}

/// A forward move a production user may make; going back takes a manager's
/// correction with a reason.
pub fn check_transition(from: Stage, to: Stage) -> ApiResult<()> {
    if to < from {
        return Err(ApiError::Conflict(
            "Moving back needs a manager's correction with a reason".into(),
        ));
    }
    if from == Stage::New && to > Stage::Templated {
        return Err(ApiError::Conflict(
            "Template the job before it goes to fabrication".into(),
        ));
    }
    Ok(())
}

/// What the caller may do with projects.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct Permissions {
    /// Sees every project.
    pub view_all: bool,
    /// Turns deals into projects (their own, or any for managers).
    pub convert: bool,
    /// Moves projects forward, links transactions, books appointments.
    pub production: bool,
    /// Confirms payment.
    pub mark_paid: bool,
    /// Cancels, puts on hold, archives, corrects stages, picks the owner.
    pub manage: bool,
}

/// The caller, as projects see them.
#[derive(Debug, Clone)]
pub struct Viewer {
    pub user_id: i32,
    pub company_id: i32,
    pub permissions: Permissions,
    /// Their own deals' projects (sales reps).
    pub own: bool,
    /// Starts projects from anyone's deals (admins, Managers, Sales Managers).
    pub convert_any: bool,
    /// Has a Projects board.
    board: bool,
}

impl Viewer {
    /// Admins, Managers, Office and Scheduling Managers (they schedule every
    /// job) and Shop Managers (they fabricate every job) see every project.
    /// Sales reps see their own deals' projects. Everyone sees the projects
    /// they have a task on, done or not: that is all a Sales or Inventory
    /// Manager sees.
    pub fn new(user: &SessionUser, positions: &[UserPositionRow]) -> Self {
        let has = |position: i32| {
            positions
                .iter()
                .any(|row| row.company_id == user.company_id && row.position_id == position)
        };
        let admin = user.is_admin || has(MANAGER);
        let sales_manager = has(SALES_MANAGER);
        let office = has(OFFICE_MANAGER);
        let shop = has(SHOP_MANAGER);
        let rep = has(SALES_REP);
        let scheduling = office || has(SCHEDULING_MANAGER);
        let tasked = sales_manager || has(INVENTORY_MANAGER);
        Self {
            user_id: user.id,
            company_id: user.company_id,
            permissions: Permissions {
                view_all: admin || scheduling || shop,
                convert: admin || sales_manager || rep,
                production: admin || scheduling || shop,
                mark_paid: admin || sales_manager,
                manage: admin || office,
            },
            own: rep,
            convert_any: admin || sales_manager,
            board: admin || rep || scheduling || shop || tasked,
        }
    }

    pub async fn load(pool: &MySqlPool, user: &SessionUser) -> ApiResult<Self> {
        let positions = user_positions(pool, user.id).await?;
        Ok(Self::new(user, &positions))
    }

    /// Has a Projects board.
    pub const fn has_board(&self) -> bool {
        self.board
    }

    /// SQL condition on `projects p`: the projects the caller sees. A task
    /// given to them keeps its project in view after it is done.
    pub fn visible_sql(&self) -> String {
        if self.permissions.view_all {
            return "TRUE".into();
        }
        let mut any = Vec::new();
        if self.own {
            any.push(format!("p.sales_rep_id = {}", self.user_id));
        }
        any.push(format!(
            "EXISTS (SELECT 1 FROM project_activities ta
                      WHERE ta.project_id = p.id AND ta.assigned_user_id = {}
                        AND ta.deleted_at IS NULL)",
            self.user_id
        ));
        format!("({})", any.join(" OR "))
    }

    pub fn require(allowed: bool) -> ApiResult<()> {
        if allowed {
            Ok(())
        } else {
            Err(ApiError::Denied(
                "Your position can't do this on a project".into(),
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user(is_admin: bool) -> SessionUser {
        SessionUser {
            id: 7,
            email: "a@b.test".into(),
            name: None,
            phone_number: None,
            is_employee: true,
            is_admin,
            is_superuser: false,
            company_id: 100,
            pined_bar: false,
            cloudtalk_agent_id: None,
            cloudtalk_phone_number: None,
            ringcentral_extension_id: None,
            ringcentral_phone_number: None,
            is_signature_available: false,
        }
    }

    fn viewer(is_admin: bool, positions: &[i32]) -> Viewer {
        let rows: Vec<UserPositionRow> = positions
            .iter()
            .map(|&position_id| UserPositionRow {
                position_id,
                company_id: 100,
            })
            .collect();
        Viewer::new(&user(is_admin), &rows)
    }

    #[test]
    fn who_has_a_board() {
        assert!(viewer(true, &[]).has_board());
        for position in [
            MANAGER,
            SALES_REP,
            SALES_MANAGER,
            SHOP_MANAGER,
            OFFICE_MANAGER,
            SCHEDULING_MANAGER,
            INVENTORY_MANAGER,
        ] {
            assert!(viewer(false, &[position]).has_board(), "{position}");
        }
        assert!(!viewer(false, &[]).has_board());
    }

    #[test]
    fn a_position_in_another_company_counts_for_nothing() {
        let rows = [UserPositionRow {
            position_id: SALES_MANAGER,
            company_id: 101,
        }];
        assert!(!Viewer::new(&user(false), &rows).has_board());
    }

    #[test]
    fn visibility_sql() {
        assert_eq!(viewer(true, &[]).visible_sql(), "TRUE");
        // The office, scheduling and the shop see every project.
        for position in [MANAGER, OFFICE_MANAGER, SCHEDULING_MANAGER, SHOP_MANAGER] {
            assert_eq!(
                viewer(false, &[position]).visible_sql(),
                "TRUE",
                "{position}"
            );
        }
        // Sales and Inventory Managers see only the projects they have a task on.
        for position in [SALES_MANAGER, INVENTORY_MANAGER] {
            let sql = viewer(false, &[position]).visible_sql();
            assert!(sql.contains("ta.assigned_user_id = 7"));
            assert!(!sql.contains("completed_at"), "done tasks keep it in view");
            assert!(!sql.contains("sales_rep_id"));
        }
        let rep = viewer(false, &[SALES_REP]).visible_sql();
        assert!(rep.contains("p.sales_rep_id = 7") && rep.contains("ta.assigned_user_id = 7"));
    }

    #[test]
    fn permissions_by_position() {
        let office = viewer(false, &[OFFICE_MANAGER]).permissions;
        assert!(office.production && office.manage && !office.mark_paid && !office.convert);
        let shop = viewer(false, &[SHOP_MANAGER]).permissions;
        assert!(shop.production && !shop.manage && !shop.mark_paid);
        let sales = viewer(false, &[SALES_MANAGER]);
        assert!(!sales.permissions.view_all && sales.convert_any);
        assert!(sales.permissions.convert && sales.permissions.mark_paid);
        let rep = viewer(false, &[SALES_REP]);
        assert!(rep.permissions.convert && !rep.convert_any && !rep.permissions.view_all);
    }

    #[test]
    fn stage_moves() {
        assert!(check_transition(Stage::New, Stage::Templated).is_ok());
        assert!(check_transition(Stage::Templated, Stage::Installed).is_ok());
        assert!(check_transition(Stage::New, Stage::Fabrication).is_err());
        assert!(check_transition(Stage::Fabrication, Stage::Templated).is_err());
        assert_eq!(
            Stage::try_from("fabrication".to_string()),
            Ok(Stage::Fabrication)
        );
        assert!(Stage::try_from("nope".to_string()).is_err());
    }
}
