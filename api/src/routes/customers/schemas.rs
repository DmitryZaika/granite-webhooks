use crate::error::ApiError;
use crate::serde_helpers::js_date;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::FromRow;
use std::collections::BTreeMap;
use utoipa::{IntoParams, ToSchema};

/// Query string of `GET /v1/customers`; same params the Remix page URL carries.
#[derive(Debug, Default, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ListQuery {
    /// `customers` (default) lists every customer. `companies` lists only
    /// customers with a company name and adds revenue and project totals.
    /// Any other value is treated as `customers`.
    #[param(value_type = Option<CustomersView>, inline)]
    pub view: Option<String>,
    /// Only customers assigned to this sales rep (a user id, see
    /// `GET /v1/users/sales-reps`). Omit or leave empty for all reps.
    #[param(value_type = Option<i32>)]
    pub sales_rep: Option<String>,
    /// `1` also returns leads marked invalid. Ignored when `view=companies`,
    /// which never filters invalid leads out.
    #[param(value_type = Option<String>, example = "1")]
    pub show_invalid: Option<String>,
}

/// Which list `GET /v1/customers` returns.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ToSchema)]
#[schema(rename_all = "lowercase")]
pub enum CustomersView {
    /// Every customer of the company.
    Customers,
    /// Only customers with a company name, plus revenue/project totals.
    Companies,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ListFilter {
    pub view: CustomersView,
    pub sales_rep: Option<i32>,
    pub include_invalid: bool,
}

impl TryFrom<ListQuery> for ListFilter {
    type Error = ApiError;

    fn try_from(query: ListQuery) -> Result<Self, ApiError> {
        let view = if query.view.as_deref() == Some("companies") {
            CustomersView::Companies
        } else {
            CustomersView::Customers
        };
        let sales_rep = match query.sales_rep.as_deref().map(str::trim) {
            None | Some("") => None,
            Some(raw) => Some(
                raw.parse::<i32>()
                    .map_err(|_| ApiError::BadRequest(format!("invalid sales_rep: {raw}")))?,
            ),
        };
        Ok(Self {
            view,
            sales_rep,
            include_invalid: query.show_invalid.as_deref() == Some("1"),
        })
    }
}

/// One row of the customers table. Field names match the Remix
/// `CustomersListCustomer` type exactly, including `customerTemperature`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, FromRow, ToSchema)]
pub struct CustomerRow {
    /// Customer id.
    pub id: i32,
    /// Contact person's name.
    pub name: Option<String>,
    /// Primary email address; all addresses come from
    /// `POST /v1/customers/emails/batch`.
    pub email: Option<String>,
    /// Main phone number.
    pub phone: Option<String>,
    /// Secondary phone number.
    pub phone_2: Option<String>,
    /// Street address.
    pub address: Option<String>,
    /// User id of the assigned sales rep.
    pub sales_rep: Option<i32>,
    /// When the customer was created.
    #[serde(serialize_with = "js_date")]
    pub created_date: Option<DateTime<Utc>>,
    /// When the current sales rep was assigned.
    #[serde(serialize_with = "js_date")]
    pub assigned_date: Option<DateTime<Utc>>,
    /// Name of the assigned sales rep; null if unassigned or the rep was deleted.
    pub sales_rep_name: Option<String>,
    /// Company (tenant) the customer belongs to.
    pub company_id: Option<i32>,
    /// Lead source, e.g. a website form or referral.
    pub source: Option<String>,
    /// Reason the lead was marked invalid. Null or empty means valid.
    pub invalid_lead: Option<String>,
    /// Business name when the customer is a company (contractor, designer...).
    pub company_name: Option<String>,
    /// Lead temperature: `hot`, `medium` or `cold`; null if not set.
    #[sqlx(rename = "customerTemperature")]
    #[serde(rename = "customerTemperature")]
    pub customer_temperature: Option<String>,
}

/// A customer row of the `companies` view, with totals over its own sales
/// and those of its child customers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, FromRow, ToSchema)]
pub struct CompanyRow {
    #[sqlx(flatten)]
    #[serde(flatten)]
    pub customer: CustomerRow,
    /// Sum of sale prices as a decimal string (e.g. `"1500.00"`), the way
    /// mysql2 returns DECIMAL; null when there are no sales.
    pub revenue_generated: Option<String>,
    /// Number of sales.
    pub projects_count: i64,
}

/// `GET /v1/customers` returns `CustomerRow`s, or `CompanyRow`s when `view=companies`.
#[derive(Debug, Serialize, ToSchema)]
#[serde(untagged)]
pub enum ListResponse {
    Customers(Vec<CustomerRow>),
    Companies(Vec<CompanyRow>),
}

/// Upper bound on ids per batch request; the customers page sends one id per row.
pub const MAX_BATCH_IDS: usize = 20_000;

/// Body of `POST /v1/customers/emails/batch`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct EmailsBatchRequest {
    /// Customer ids to look up, at most 20000. Ids of other companies are ignored.
    pub customer_ids: Vec<i32>,
}

/// `{ "<customer_id>": ["primary@x.com", "other@x.com"] }`. Customers with no
/// email rows are absent.
pub type EmailsByCustomer = BTreeMap<i32, Vec<String>>;
