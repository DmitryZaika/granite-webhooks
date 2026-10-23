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

/// Query string of `GET /v1/customers/page`: the customers page URL's params.
#[derive(Debug, Default, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct PageQuery {
    /// As in `GET /v1/customers`.
    #[param(value_type = Option<CustomersView>, inline)]
    pub view: Option<String>,
    /// As in `GET /v1/customers`.
    #[param(value_type = Option<i32>)]
    pub sales_rep: Option<String>,
    /// As in `GET /v1/customers`.
    #[param(value_type = Option<String>, example = "1")]
    pub show_invalid: Option<String>,
    /// `walkin` (default), `leads`, `call-in`, `other` (with `user-input`) or
    /// `all`. Any other value matches no customers.
    #[param(example = "leads")]
    pub tab: Option<String>,
    /// Sort column: `name`, `phone`, `email`, `sales_rep_name`,
    /// `company_name`, `created_date`, `assigned_date`, and in the companies
    /// view `revenue_generated`, `projects_count`. Omitted or unknown: newest
    /// first by the tab's date (leads by when they were assigned).
    pub sort_by: Option<String>,
    /// `asc` (default) or `desc`.
    #[param(example = "desc")]
    pub order: Option<String>,
    /// 1-based page, default 1. Past the last page, the last page is returned.
    #[param(value_type = Option<u32>)]
    pub page: Option<String>,
    /// Rows per page, default 20, at most 500.
    #[param(value_type = Option<u32>)]
    pub page_size: Option<String>,
}

/// The customers page's tabs, by lead source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CustomersTab {
    Walkin,
    Leads,
    CallIn,
    Other,
    All,
}

/// How a page is ordered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PageSort {
    /// Newest first by the tab's date.
    Default,
    /// By a text column, case-insensitively; empty sorts as "".
    Text(&'static str),
    /// By the day created.
    Created,
    /// By the day assigned, or created when unassigned.
    Assigned,
    /// By a number column of the companies view.
    Number(&'static str),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PageRequest {
    pub filter: ListFilter,
    /// `None` for an unknown tab: no rows.
    pub tab: Option<CustomersTab>,
    pub sort: PageSort,
    pub descending: bool,
    pub page: u32,
    pub page_size: u32,
}

pub const DEFAULT_PAGE_SIZE: u32 = 20;
pub const MAX_PAGE_SIZE: u32 = 500;

impl TryFrom<PageQuery> for PageRequest {
    type Error = ApiError;

    fn try_from(query: PageQuery) -> Result<Self, ApiError> {
        let filter = ListFilter::try_from(ListQuery {
            view: query.view,
            sales_rep: query.sales_rep,
            show_invalid: query.show_invalid,
        })?;
        let tab = match query.tab.as_deref().unwrap_or("walkin") {
            "walkin" => Some(CustomersTab::Walkin),
            "leads" => Some(CustomersTab::Leads),
            "call-in" => Some(CustomersTab::CallIn),
            "other" => Some(CustomersTab::Other),
            "all" => Some(CustomersTab::All),
            _ => None,
        };
        let companies = filter.view == CustomersView::Companies;
        let sort = match query.sort_by.as_deref() {
            Some(
                column @ ("name" | "phone" | "email" | "sales_rep_name" | "company_name"),
            ) => PageSort::Text(column_name(column)),
            Some("created_date") => PageSort::Created,
            Some("assigned_date") => PageSort::Assigned,
            Some(column @ ("revenue_generated" | "projects_count")) if companies => {
                PageSort::Number(column_name(column))
            }
            _ => PageSort::Default,
        };
        Ok(Self {
            filter,
            tab,
            sort,
            descending: query.order.as_deref().unwrap_or("asc") != "asc",
            page: positive(query.page.as_deref()).unwrap_or(1),
            page_size: positive(query.page_size.as_deref())
                .unwrap_or(DEFAULT_PAGE_SIZE)
                .min(MAX_PAGE_SIZE),
        })
    }
}

/// The column as a `'static` name, safe to put in SQL.
fn column_name(column: &str) -> &'static str {
    match column {
        "name" => "name",
        "phone" => "phone",
        "email" => "email",
        "sales_rep_name" => "sales_rep_name",
        "company_name" => "company_name",
        "revenue_generated" => "revenue_generated",
        _ => "projects_count",
    }
}

/// A whole number above 0; anything else falls back to the default.
fn positive(raw: Option<&str>) -> Option<u32> {
    raw?.trim().parse::<u32>().ok().filter(|n| *n > 0)
}

/// One page of the customers table.
#[derive(Debug, Serialize, ToSchema)]
pub struct CustomersPage {
    /// The page's rows, shaped as in `GET /v1/customers`.
    pub rows: ListResponse,
    /// Every email of the page's customers, primary first, keyed by customer id.
    #[schema(value_type = BTreeMap<String, Vec<String>>)]
    pub emails: EmailsByCustomer,
    /// Rows over all pages.
    pub total: i64,
    /// The page returned: the last one when `page` was past it.
    pub page: u32,
    /// For the "this month" counts per sales rep: the tab's customers whose
    /// tab date falls in the current UTC month. Empty in the companies view
    /// and the `other` tab, which show no counts.
    pub month_rows: Vec<CustomerRow>,
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

/// One customer with contact details, as the customer picker shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, FromRow, ToSchema)]
pub struct CustomerDetail {
    /// Customer id.
    pub id: i32,
    /// Contact person's name.
    pub name: Option<String>,
    /// Billing address.
    pub address: Option<String>,
    /// Main phone number.
    pub phone: Option<String>,
    /// Second phone number.
    pub phone_2: Option<String>,
    /// Primary email address; null when the customer has none.
    pub email: Option<String>,
    /// Every email address, primary first.
    #[sqlx(skip)]
    pub emails: Vec<String>,
    /// Company name for business customers; null or empty for private ones.
    pub company_name: Option<String>,
    /// Where the lead came from, e.g. `leads`, `check-in`, `other`.
    pub source: Option<String>,
    /// Referral source, free text.
    pub referral_source: Option<String>,
    /// Message the customer left when signing up.
    pub your_message: Option<String>,
    /// Assigned sales rep (user id); null when unassigned.
    pub sales_rep: Option<i32>,
    /// Name of the assigned sales rep; null when unassigned or deleted.
    pub sales_rep_name: Option<String>,
    /// City of the address.
    pub city: Option<String>,
    /// State of the address.
    pub state: Option<String>,
    /// ZIP code.
    pub postal_code: Option<String>,
    /// Lead temperature: `hot`, `medium`, `cold`, or null.
    pub customer_temperature: Option<String>,
    /// Internal notes about the customer.
    pub notes: Option<String>,
    /// QuickBooks customer id when linked.
    pub qbo_id: Option<i32>,
    /// File the lead attached to the web form (URL).
    pub attached_file: Option<String>,
    /// When the customer was created.
    #[serde(serialize_with = "js_date")]
    #[schema(value_type = Option<String>, format = DateTime)]
    pub created_date: Option<DateTime<Utc>>,
    /// Why the lead was marked invalid; null or empty for a valid one.
    pub invalid_lead: Option<String>,
    /// Lead form answers, as the lead sources send them (free text).
    #[sqlx(flatten)]
    pub lead: LeadAnswers,
}

/// What a lead answered on the ad or web form; every field free text, null
/// when not asked.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, FromRow, ToSchema)]
pub struct LeadAnswers {
    /// Kind of remodel (kitchen, bath...).
    pub remodal_type: Option<String>,
    /// Project size.
    pub project_size: Option<String>,
    /// Best time to call.
    pub contact_time: Option<String>,
    /// Whether old countertops must be torn out and disposed of.
    pub remove_and_dispose: Option<String>,
    /// Asked for a better offer (1 yes, 0 no).
    pub improve_offer: Option<i8>,
    /// Sink wanted.
    pub sink: Option<String>,
    /// When the project should start.
    pub when_start: Option<String>,
    /// Project details.
    pub details: Option<String>,
    /// Ad campaign name.
    pub compaign_name: Option<String>,
    /// Ad set name.
    pub adset_name: Option<String>,
    /// Ad name.
    pub ad_name: Option<String>,
    /// Backsplash wanted.
    pub backsplash: Option<String>,
    /// Stove type.
    pub kitchen_stove: Option<String>,
}

/// Query of `GET /v1/customers/{id}`.
#[derive(Debug, Default, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct CustomerQuery {
    /// Also return the customer when it was deleted.
    #[serde(default)]
    pub include_deleted: bool,
}

/// How `GET /v1/customers/search` matches `term`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum CustomerSearchBy {
    /// Person or company name, tolerating typos.
    #[default]
    Name,
    /// Phone or second phone, by digits (formatting ignored).
    Phone,
    /// Any of the customer's email addresses.
    Email,
    /// Company name, tolerating typos.
    Company,
    /// Every word typed appears in the address ("123 main" finds "123 N Main St").
    Address,
}

/// Query string of `GET /v1/customers/search`.
#[derive(Debug, Default, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct CustomerSearchQuery {
    /// What the user typed. Required (may be empty for phone search).
    pub term: String,
    /// Which field to match: `name` (default), `phone`, `email`, `company`, `address`.
    #[serde(default)]
    #[param(inline)]
    pub by: CustomerSearchBy,
    /// `true` returns only customers assigned to the caller or unassigned.
    #[serde(default)]
    pub own_only: bool,
}

/// A customer matching a search, with their latest deal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, FromRow, ToSchema)]
pub struct CustomerSearchRow {
    /// Customer id.
    pub id: i32,
    /// Contact person's name.
    pub name: Option<String>,
    /// Billing address.
    pub address: Option<String>,
    /// City of the address.
    pub city: Option<String>,
    /// State of the address.
    pub state: Option<String>,
    /// ZIP code.
    pub postal_code: Option<String>,
    /// Main phone number.
    pub phone: Option<String>,
    /// Second phone number.
    pub phone_2: Option<String>,
    /// Primary email address.
    pub email: Option<String>,
    /// Every email address, primary first.
    #[sqlx(skip)]
    pub emails: Vec<String>,
    /// Company name for business customers.
    pub company_name: Option<String>,
    /// The customer's most recent live deal; null when none.
    pub deal_id: Option<i64>,
    /// That deal's outcome: 1 won, 0 lost, null open or no deal.
    pub deal_is_won: Option<i32>,
    /// Board group of that deal's list; null when there is no deal.
    pub deal_group_id: Option<i32>,
}
