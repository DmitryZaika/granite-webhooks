use crate::openapi::ToSchema;
use serde::Serialize;
use sqlx::FromRow;

pub use crate::sms_provider::SmsProvider;

/// The signed-in user's deals this month so far, from the 1st through today
/// (UTC). Only deals assigned to the user, of live customers of the company,
/// in a live deal list, count.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, FromRow, ToSchema)]
pub struct SalesStats {
    /// First day counted, `YYYY-MM-DD`.
    pub start_date: String,
    /// Last day counted (today), `YYYY-MM-DD`.
    pub end_date: String,
    /// Deals marked won this month (dated by their last change).
    pub won_count: i64,
    /// Deals marked lost this month.
    pub lost_count: i64,
    /// Total amount of the deals won this month, decimal string in USD
    /// (e.g. "2200.00"); "0.00" when none.
    pub won_amount: String,
    /// Open deals (neither won nor lost), whenever they were created.
    pub active_deals_count: i64,
    /// Total amount of the open deals, decimal string in USD; deals without an
    /// amount add nothing.
    pub pipeline_amount: String,
    /// Deals created this month.
    pub new_deals_count: i64,
}

/// How many of the signed-in user's deals were won, overall and by how the
/// customer came in. All time; deals of deleted customers or in deleted lists
/// are left out.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct DealConversion {
    /// Every deal.
    pub total: ConversionCount,
    /// Customers from leads (`source = leads`).
    pub leads: ConversionCount,
    /// Walk-in customers (`source = check-in`).
    pub walk_in: ConversionCount,
    /// Customers who called in (`source = call-in`).
    pub call_in: ConversionCount,
}

/// Won deals out of all deals of one group.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
pub struct ConversionCount {
    /// Deals marked won.
    pub won: i64,
    /// All deals, whatever their state.
    pub created: i64,
}

#[derive(Debug, Clone, Copy, FromRow)]
pub struct ConversionRow {
    pub total_won: i64,
    pub total_created: i64,
    pub leads_won: i64,
    pub leads_created: i64,
    pub walk_in_won: i64,
    pub walk_in_created: i64,
    pub call_in_won: i64,
    pub call_in_created: i64,
}

impl From<ConversionRow> for DealConversion {
    fn from(row: ConversionRow) -> Self {
        Self {
            total: ConversionCount {
                won: row.total_won,
                created: row.total_created,
            },
            leads: ConversionCount {
                won: row.leads_won,
                created: row.leads_created,
            },
            walk_in: ConversionCount {
                won: row.walk_in_won,
                created: row.walk_in_created,
            },
            call_in: ConversionCount {
                won: row.call_in_won,
                created: row.call_in_created,
            },
        }
    }
}

/// A stone the signed-in user sold slabs of this month.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, FromRow, ToSchema)]
pub struct TopSoldStone {
    /// Stone id.
    pub stone_id: i32,
    /// Stone name, e.g. "Absolute Black".
    pub stone_name: String,
    /// Full slabs sold (remnants cut from them are not counted again).
    pub slabs_sold: i64,
    /// Square feet of those slabs, decimal string; "0.00" when no slab has a size.
    pub square_feet: String,
}

/// How many messages were sent in one calendar month.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, FromRow, ToSchema)]
pub struct MonthlyCount {
    /// Month, `YYYY-MM`.
    pub month: String,
    /// Messages sent that month.
    pub count: i64,
}

/// Texts the signed-in user sent through the company's SMS provider.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct SentSms {
    /// The provider counted; null when the company has none, and then
    /// `months` is empty.
    pub provider: Option<SmsProvider>,
    /// Months with at least one text, oldest first, over the current month and
    /// the two before it. Months without texts are left out.
    pub months: Vec<MonthlyCount>,
}
