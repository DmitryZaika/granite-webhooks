use crate::error::ApiError;
use crate::serde_helpers::js_date;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::FromRow;
use std::collections::BTreeMap;

/// Query string of `GET /v1/customers`; same params the Remix page URL carries.
#[derive(Debug, Default, Deserialize)]
pub struct ListQuery {
    pub view: Option<String>,
    pub sales_rep: Option<String>,
    pub show_invalid: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CustomersView {
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, FromRow)]
pub struct CustomerRow {
    pub id: i32,
    pub name: Option<String>,
    pub email: Option<String>,
    pub phone: Option<String>,
    pub phone_2: Option<String>,
    pub address: Option<String>,
    pub sales_rep: Option<i32>,
    #[serde(serialize_with = "js_date")]
    pub created_date: Option<DateTime<Utc>>,
    #[serde(serialize_with = "js_date")]
    pub assigned_date: Option<DateTime<Utc>>,
    pub sales_rep_name: Option<String>,
    pub company_id: Option<i32>,
    pub source: Option<String>,
    pub invalid_lead: Option<String>,
    pub company_name: Option<String>,
    #[sqlx(rename = "customerTemperature")]
    #[serde(rename = "customerTemperature")]
    pub customer_temperature: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, FromRow)]
pub struct CompanyRow {
    #[sqlx(flatten)]
    #[serde(flatten)]
    pub customer: CustomerRow,
    /// Decimal as a string (e.g. `"1500.00"`), the way mysql2 returns DECIMAL.
    pub revenue_generated: Option<String>,
    pub projects_count: i64,
}

#[derive(Debug, Serialize)]
#[serde(untagged)]
pub enum ListResponse {
    Customers(Vec<CustomerRow>),
    Companies(Vec<CompanyRow>),
}

/// Upper bound on ids per batch request; the customers page sends one id per row.
pub const MAX_BATCH_IDS: usize = 20_000;

#[derive(Debug, Deserialize)]
pub struct EmailsBatchRequest {
    pub customer_ids: Vec<i32>,
}

/// `{ "<customer_id>": ["primary@x.com", "other@x.com"] }`. Customers with no
/// email rows are absent.
pub type EmailsByCustomer = BTreeMap<i32, Vec<String>>;
