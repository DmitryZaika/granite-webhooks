use crate::openapi::ToSchema;
use serde::{Deserialize, Serialize};

/// A widget of the employee dashboard. Must match `EmployeeDashboardWidgetId`
/// in the frontend (`app/utils/employeeDashboardWidgets.ts`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "kebab-case")]
pub enum DashboardWidget {
    /// Unread emails and texts (shown in the page header).
    UnreadMessages,
    /// The points leaderboard (the grid's fixed first block).
    Leaderboard,
    /// Coaching on recent calls; needs a CloudTalk or RingCentral line.
    CallCoaching,
    /// QuickBooks sales this month against the monthly goal.
    QboSales,
    /// Won deals this month against the monthly goal (without QuickBooks).
    WonDeals,
    /// Deals won, lost, open and new this month.
    MySales,
    /// Won deals out of all, by customer source.
    ConversionMetrics,
    /// Shortcuts to other pages.
    QuickLinks,
    /// Setup checklist and guided tours.
    Onboarding,
    /// Stones the user sold most this month.
    TopSoldColors,
    /// Emails sent per month.
    EmailsPerMonth,
    /// Texts sent per month.
    SmsPerMonth,
    /// The company's sales today: order, rep, stone, sinks, package, amount, deposit.
    DailySales,
    /// Stones with slabs not tied to a sale.
    AvailableInventory,
}

impl DashboardWidget {
    pub const fn id(self) -> &'static str {
        match self {
            Self::UnreadMessages => "unread-messages",
            Self::Leaderboard => "leaderboard",
            Self::CallCoaching => "call-coaching",
            Self::QboSales => "qbo-sales",
            Self::WonDeals => "won-deals",
            Self::MySales => "my-sales",
            Self::ConversionMetrics => "conversion-metrics",
            Self::QuickLinks => "quick-links",
            Self::Onboarding => "onboarding",
            Self::TopSoldColors => "top-sold-colors",
            Self::EmailsPerMonth => "emails-per-month",
            Self::SmsPerMonth => "sms-per-month",
            Self::DailySales => "daily-sales",
            Self::AvailableInventory => "available-inventory",
        }
    }

    pub fn from_id(id: &str) -> Option<Self> {
        serde_json::from_value(serde_json::Value::String(id.to_string())).ok()
    }
}

/// The signed-in user's dashboard layout in the company the request acts in.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct DashboardLayout {
    /// Widgets in screen order. Empty when the user never saved a layout in
    /// this company (show the default widgets) or removed every widget.
    pub widget_ids: Vec<DashboardWidget>,
}

/// What a user did with a widget.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum WidgetAction {
    /// Clicked something in it (`target` says what).
    Click,
    /// Changed a value in it, e.g. the sales goal.
    Change,
    /// Added it to the dashboard.
    Add,
    /// Removed it from the dashboard.
    Remove,
}

impl WidgetAction {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Click => "click",
            Self::Change => "change",
            Self::Add => "add",
            Self::Remove => "remove",
        }
    }
}

/// One use of a dashboard widget, for product analytics.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct WidgetEvent {
    /// The widget used.
    pub widget_id: DashboardWidget,
    /// What was done.
    pub action: WidgetAction,
    /// What in the widget was used, e.g. `quick-links` or `sales-goal`;
    /// 1 to 64 characters after trimming.
    pub target: String,
}
