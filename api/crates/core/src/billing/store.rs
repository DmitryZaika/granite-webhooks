//! Where billing state lives: `company_billing`, `company_billing_invoices`,
//! `stripe_webhook_events` and `billing_notifications`. A trait so the state
//! machine can be tested in memory.

use super::{
    BillingInvoice, BillingNotification, CompanyBilling, MANAGER_POSITION, MethodHold, MethodState,
    PaymentStatus,
};
use chrono::NaiveDateTime;
use sqlx::{FromRow, MySqlPool};

pub type StoreResult<T> = Result<T, sqlx::Error>;

#[allow(async_fn_in_trait)] // used with concrete types only
pub trait BillingStore {
    async fn by_company(&self, company_id: i32) -> StoreResult<Option<CompanyBilling>>;
    async fn by_customer(&self, customer_id: &str) -> StoreResult<Option<CompanyBilling>>;
    async fn by_subscription(&self, subscription_id: &str) -> StoreResult<Option<CompanyBilling>>;
    async fn save(&self, record: &CompanyBilling) -> StoreResult<()>;
    /// Records the event, unless it was already processed or another
    /// delivery is processing it. A claim left unfinished for 5 minutes (a
    /// crash, a timeout) is taken over.
    async fn claim_event(&self, event_id: &str, event_type: &str) -> StoreResult<Claim>;
    /// The claimed event's work is done: later deliveries are duplicates.
    async fn finish_event(&self, event_id: &str) -> StoreResult<()>;
    async fn release_event(&self, event_id: &str) -> StoreResult<()>;
    async fn upsert_invoice(&self, invoice: &BillingInvoice) -> StoreResult<()>;
    async fn invoices(&self, company_id: i32) -> StoreResult<Vec<BillingInvoice>>;
    /// Users of the company with the Manager position in it, by id.
    async fn managers(&self, company_id: i32) -> StoreResult<Vec<i32>>;
    /// False when the same notification (dedupe key) already exists.
    async fn insert_notification(&self, notification: &BillingNotification) -> StoreResult<bool>;
    async fn clear_payment_notifications(&self, company_id: i32) -> StoreResult<()>;
    /// `company.billing_required`.
    async fn billing_required(&self, company_id: i32) -> StoreResult<bool>;
    /// `company.billing_required_at`: when Stripe payment was last turned on.
    async fn billing_required_since(&self, company_id: i32) -> StoreResult<Option<NaiveDateTime>>;
    /// `company.plan_source` is `subscription` (a self-serve plan).
    async fn self_serve(&self, company_id: i32) -> StoreResult<bool>;
    /// The status of the company's plan invite `invite_id` (`open`,
    /// `accepting`, `accepted`, `revoked`); `None` when it has no such invite.
    async fn plan_invite_status(
        &self,
        company_id: i32,
        invite_id: &str,
    ) -> StoreResult<Option<String>>;
    /// `bank_verification_until` of the company's signup still waiting for
    /// its bank account (`awaiting_bank_verification`); `None` without one.
    async fn bank_verification_until(&self, company_id: i32)
    -> StoreResult<Option<NaiveDateTime>>;
}

/// What [`BillingStore::claim_event`] found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Claim {
    /// This delivery applies the event.
    Taken,
    /// Already processed: a duplicate.
    Done,
    /// Another delivery is processing it (or crashed less than
    /// [`STALE_CLAIM_MINUTES`] ago): Stripe has to retry this one.
    InProgress,
}

/// How old an unfinished event claim must be before a new delivery takes it
/// over (longer than any webhook run).
pub const STALE_CLAIM_MINUTES: i64 = 5;

/// The MySQL tables.
pub struct MySqlBillingStore<'a>(pub &'a MySqlPool);

#[derive(FromRow)]
struct BillingRow {
    company_id: i32,
    stripe_customer_id: Option<String>,
    stripe_subscription_id: Option<String>,
    stripe_price_id: Option<String>,
    stripe_subscription_item_id: Option<String>,
    stripe_payment_method_id: Option<String>,
    subscription_status: Option<String>,
    payment_status: Option<String>,
    current_period_start: Option<NaiveDateTime>,
    current_period_end: Option<NaiveDateTime>,
    last_successful_payment_at: Option<NaiveDateTime>,
    failed_payment_at: Option<NaiveDateTime>,
    grace_period_ends_at: Option<NaiveDateTime>,
    plan_name: Option<String>,
    monthly_price_cents: Option<i64>,
    user_count: Option<i64>,
    outstanding_balance_cents: Option<i64>,
    recovered_failed_at: Option<NaiveDateTime>,
    recovered_at: Option<NaiveDateTime>,
    trial_end_at: Option<NaiveDateTime>,
    extra_user_item_id: Option<String>,
    billable_users: Option<i64>,
    cancel_at: Option<NaiveDateTime>,
    pending_setup_intent_id: Option<String>,
    method_hold: Option<String>,
    method_hold_at: Option<NaiveDateTime>,
    method_hold_until: Option<NaiveDateTime>,
    payment_processing_at: Option<NaiveDateTime>,
}

/// Blank strings read as missing, like the CRM's `readString`.
fn present(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

impl From<BillingRow> for CompanyBilling {
    fn from(row: BillingRow) -> Self {
        Self {
            company_id: row.company_id,
            stripe_customer_id: present(row.stripe_customer_id),
            stripe_subscription_id: present(row.stripe_subscription_id),
            stripe_price_id: present(row.stripe_price_id),
            stripe_subscription_item_id: present(row.stripe_subscription_item_id),
            stripe_payment_method_id: present(row.stripe_payment_method_id),
            subscription_status: present(row.subscription_status).unwrap_or_else(|| "none".into()),
            payment_status: PaymentStatus::parse(row.payment_status.as_deref().unwrap_or_default()),
            current_period_start: row.current_period_start,
            current_period_end: row.current_period_end,
            last_successful_payment_at: row.last_successful_payment_at,
            failed_payment_at: row.failed_payment_at,
            grace_period_ends_at: row.grace_period_ends_at,
            plan_name: present(row.plan_name),
            monthly_price_cents: row.monthly_price_cents,
            user_count: row.user_count,
            outstanding_balance_cents: row.outstanding_balance_cents.unwrap_or_default(),
            recovered_failed_at: row.recovered_failed_at,
            recovered_at: row.recovered_at,
            trial_end_at: row.trial_end_at,
            extra_user_item_id: present(row.extra_user_item_id),
            billable_users: row.billable_users,
            cancel_at: row.cancel_at,
            methods: MethodState {
                pending_setup_intent_id: present(row.pending_setup_intent_id),
                hold: row.method_hold.as_deref().and_then(MethodHold::parse),
                hold_at: row.method_hold_at,
                hold_until: row.method_hold_until,
                payment_processing_at: row.payment_processing_at,
            },
        }
    }
}

const BILLING_SELECT: &str =
    "SELECT company_id, stripe_customer_id, stripe_subscription_id, stripe_price_id,
        stripe_subscription_item_id, stripe_payment_method_id, subscription_status, payment_status,
        current_period_start, current_period_end, last_successful_payment_at, failed_payment_at,
        grace_period_ends_at, plan_name, CAST(monthly_price_cents AS SIGNED) AS monthly_price_cents,
        CAST(user_count AS SIGNED) AS user_count,
        CAST(outstanding_balance_cents AS SIGNED) AS outstanding_balance_cents,
        recovered_failed_at, recovered_at, trial_end_at, extra_user_item_id,
        CAST(billable_users AS SIGNED) AS billable_users, cancel_at, pending_setup_intent_id,
        CAST(method_hold AS CHAR) AS method_hold, method_hold_at, method_hold_until,
        payment_processing_at
   FROM company_billing";

#[derive(FromRow)]
struct InvoiceRow {
    company_id: i32,
    stripe_invoice_id: String,
    status: String,
    amount_due_cents: i64,
    amount_paid_cents: i64,
    currency: String,
    hosted_invoice_url: Option<String>,
    invoice_pdf: Option<String>,
    paid_at: Option<NaiveDateTime>,
}

fn is_duplicate(err: &sqlx::Error) -> bool {
    err.as_database_error()
        .is_some_and(sqlx::error::DatabaseError::is_unique_violation)
}

impl MySqlBillingStore<'_> {
    async fn one(&self, column: &str, value: &str) -> StoreResult<Option<CompanyBilling>> {
        let row: Option<BillingRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "{BILLING_SELECT} WHERE {column} = ? LIMIT 1"
        )))
        .bind(value)
        .fetch_optional(self.0)
        .await?;
        Ok(row.map(CompanyBilling::from))
    }
}

impl BillingStore for MySqlBillingStore<'_> {
    async fn by_company(&self, company_id: i32) -> StoreResult<Option<CompanyBilling>> {
        let row: Option<BillingRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "{BILLING_SELECT} WHERE company_id = ? LIMIT 1"
        )))
        .bind(company_id)
        .fetch_optional(self.0)
        .await?;
        Ok(row.map(CompanyBilling::from))
    }

    async fn by_customer(&self, customer_id: &str) -> StoreResult<Option<CompanyBilling>> {
        self.one("stripe_customer_id", customer_id).await
    }

    async fn by_subscription(&self, subscription_id: &str) -> StoreResult<Option<CompanyBilling>> {
        self.one("stripe_subscription_id", subscription_id).await
    }

    async fn save(&self, record: &CompanyBilling) -> StoreResult<()> {
        sqlx::query(
            "INSERT INTO company_billing (
               company_id, stripe_customer_id, stripe_subscription_id, stripe_price_id,
               stripe_subscription_item_id, stripe_payment_method_id, subscription_status,
               payment_status, current_period_start, current_period_end, last_successful_payment_at,
               failed_payment_at, grace_period_ends_at, plan_name, monthly_price_cents, user_count,
               outstanding_balance_cents, recovered_failed_at, recovered_at, trial_end_at,
               extra_user_item_id, billable_users, cancel_at
             ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
             ON DUPLICATE KEY UPDATE
               stripe_customer_id = VALUES(stripe_customer_id),
               stripe_subscription_id = VALUES(stripe_subscription_id),
               stripe_price_id = VALUES(stripe_price_id),
               stripe_subscription_item_id = VALUES(stripe_subscription_item_id),
               stripe_payment_method_id = VALUES(stripe_payment_method_id),
               subscription_status = VALUES(subscription_status),
               payment_status = VALUES(payment_status),
               current_period_start = VALUES(current_period_start),
               current_period_end = VALUES(current_period_end),
               last_successful_payment_at = VALUES(last_successful_payment_at),
               failed_payment_at = VALUES(failed_payment_at),
               grace_period_ends_at = VALUES(grace_period_ends_at),
               plan_name = VALUES(plan_name),
               monthly_price_cents = VALUES(monthly_price_cents),
               user_count = VALUES(user_count),
               outstanding_balance_cents = VALUES(outstanding_balance_cents),
               recovered_failed_at = VALUES(recovered_failed_at),
               recovered_at = VALUES(recovered_at),
               trial_end_at = VALUES(trial_end_at),
               extra_user_item_id = VALUES(extra_user_item_id),
               billable_users = VALUES(billable_users),
               cancel_at = VALUES(cancel_at)",
        )
        .bind(record.company_id)
        .bind(&record.stripe_customer_id)
        .bind(&record.stripe_subscription_id)
        .bind(&record.stripe_price_id)
        .bind(&record.stripe_subscription_item_id)
        .bind(&record.stripe_payment_method_id)
        .bind(&record.subscription_status)
        .bind(record.payment_status.as_str())
        .bind(record.current_period_start)
        .bind(record.current_period_end)
        .bind(record.last_successful_payment_at)
        .bind(record.failed_payment_at)
        .bind(record.grace_period_ends_at)
        .bind(&record.plan_name)
        .bind(record.monthly_price_cents)
        .bind(record.user_count)
        .bind(record.outstanding_balance_cents)
        .bind(record.recovered_failed_at)
        .bind(record.recovered_at)
        .bind(record.trial_end_at)
        .bind(&record.extra_user_item_id)
        .bind(record.billable_users)
        .bind(record.cancel_at)
        .execute(self.0)
        .await?;
        Ok(())
    }

    async fn claim_event(&self, event_id: &str, event_type: &str) -> StoreResult<Claim> {
        match sqlx::query(
            "INSERT INTO stripe_webhook_events (stripe_event_id, event_type) VALUES (?, ?)",
        )
        .bind(event_id)
        .bind(event_type)
        .execute(self.0)
        .await
        {
            Ok(_) => Ok(Claim::Taken),
            Err(err) if is_duplicate(&err) => {
                let taken = sqlx::query(sqlx::AssertSqlSafe(format!(
                    "UPDATE stripe_webhook_events SET processed_at = CURRENT_TIMESTAMP
                      WHERE stripe_event_id = ? AND done_at IS NULL
                        AND processed_at < CURRENT_TIMESTAMP - INTERVAL {STALE_CLAIM_MINUTES} MINUTE"
                )))
                .bind(event_id)
                .execute(self.0)
                .await?;
                if taken.rows_affected() == 1 {
                    return Ok(Claim::Taken);
                }
                let done: Option<bool> = sqlx::query_scalar(
                    "SELECT done_at IS NOT NULL FROM stripe_webhook_events
                      WHERE stripe_event_id = ?",
                )
                .bind(event_id)
                .fetch_optional(self.0)
                .await?;
                // A row gone meanwhile (the other delivery failed and released
                // it) is retried as well.
                Ok(if done == Some(true) {
                    Claim::Done
                } else {
                    Claim::InProgress
                })
            }
            Err(err) => Err(err),
        }
    }

    async fn finish_event(&self, event_id: &str) -> StoreResult<()> {
        sqlx::query(
            "UPDATE stripe_webhook_events SET done_at = CURRENT_TIMESTAMP WHERE stripe_event_id = ?",
        )
        .bind(event_id)
        .execute(self.0)
        .await?;
        Ok(())
    }

    async fn release_event(&self, event_id: &str) -> StoreResult<()> {
        sqlx::query("DELETE FROM stripe_webhook_events WHERE stripe_event_id = ?")
            .bind(event_id)
            .execute(self.0)
            .await?;
        Ok(())
    }

    async fn upsert_invoice(&self, invoice: &BillingInvoice) -> StoreResult<()> {
        sqlx::query(
            "INSERT INTO company_billing_invoices (
               company_id, stripe_invoice_id, status, amount_due_cents, amount_paid_cents,
               currency, hosted_invoice_url, invoice_pdf, paid_at
             ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
             ON DUPLICATE KEY UPDATE
               company_id = VALUES(company_id),
               status = VALUES(status),
               amount_due_cents = VALUES(amount_due_cents),
               amount_paid_cents = VALUES(amount_paid_cents),
               currency = VALUES(currency),
               hosted_invoice_url = VALUES(hosted_invoice_url),
               invoice_pdf = VALUES(invoice_pdf),
               paid_at = VALUES(paid_at)",
        )
        .bind(invoice.company_id)
        .bind(&invoice.stripe_invoice_id)
        .bind(&invoice.status)
        .bind(invoice.amount_due_cents)
        .bind(invoice.amount_paid_cents)
        .bind(&invoice.currency)
        .bind(&invoice.hosted_invoice_url)
        .bind(&invoice.invoice_pdf)
        .bind(invoice.paid_at)
        .execute(self.0)
        .await?;
        Ok(())
    }

    async fn invoices(&self, company_id: i32) -> StoreResult<Vec<BillingInvoice>> {
        let rows: Vec<InvoiceRow> = sqlx::query_as(
            "SELECT company_id, stripe_invoice_id, status,
                    CAST(amount_due_cents AS SIGNED) AS amount_due_cents,
                    CAST(amount_paid_cents AS SIGNED) AS amount_paid_cents,
                    currency, hosted_invoice_url, invoice_pdf, paid_at
               FROM company_billing_invoices
              WHERE company_id = ?
              ORDER BY paid_at IS NULL, paid_at DESC, id DESC",
        )
        .bind(company_id)
        .fetch_all(self.0)
        .await?;
        Ok(rows
            .into_iter()
            .map(|row| BillingInvoice {
                company_id: row.company_id,
                stripe_invoice_id: row.stripe_invoice_id,
                status: row.status,
                amount_due_cents: row.amount_due_cents,
                amount_paid_cents: row.amount_paid_cents,
                currency: present(Some(row.currency)).unwrap_or_else(|| "usd".into()),
                hosted_invoice_url: present(row.hosted_invoice_url),
                invoice_pdf: present(row.invoice_pdf),
                paid_at: row.paid_at,
            })
            .collect())
    }

    async fn managers(&self, company_id: i32) -> StoreResult<Vec<i32>> {
        sqlx::query_scalar(
            "SELECT u.id FROM users u
              WHERE u.company_id = ? AND u.is_deleted = 0
                AND EXISTS (SELECT 1 FROM users_positions up
                             WHERE up.user_id = u.id AND up.company_id = u.company_id
                               AND up.position_id = ?)
              ORDER BY u.id",
        )
        .bind(company_id)
        .bind(MANAGER_POSITION)
        .fetch_all(self.0)
        .await
    }

    async fn insert_notification(&self, notification: &BillingNotification) -> StoreResult<bool> {
        match sqlx::query(
            "INSERT INTO billing_notifications (
               company_id, user_id, dedupe_key, title, message, action_label, href
             ) VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(notification.company_id)
        .bind(notification.user_id)
        .bind(&notification.dedupe_key)
        .bind(&notification.title)
        .bind(&notification.message)
        .bind(&notification.action_label)
        .bind(&notification.href)
        .execute(self.0)
        .await
        {
            Ok(_) => Ok(true),
            Err(err) if is_duplicate(&err) => Ok(false),
            Err(err) => Err(err),
        }
    }

    async fn clear_payment_notifications(&self, company_id: i32) -> StoreResult<()> {
        sqlx::query(
            "UPDATE billing_notifications SET is_done = 1
              WHERE company_id = ? AND is_done = 0 AND dedupe_key LIKE 'payment_required:%'",
        )
        .bind(company_id)
        .execute(self.0)
        .await?;
        Ok(())
    }

    async fn billing_required(&self, company_id: i32) -> StoreResult<bool> {
        let required: Option<bool> =
            sqlx::query_scalar("SELECT billing_required <> 0 FROM company WHERE id = ? LIMIT 1")
                .bind(company_id)
                .fetch_optional(self.0)
                .await?;
        Ok(required.unwrap_or(false))
    }

    async fn billing_required_since(&self, company_id: i32) -> StoreResult<Option<NaiveDateTime>> {
        let since: Option<Option<NaiveDateTime>> =
            sqlx::query_scalar("SELECT billing_required_at FROM company WHERE id = ? LIMIT 1")
                .bind(company_id)
                .fetch_optional(self.0)
                .await?;
        Ok(since.flatten())
    }

    async fn self_serve(&self, company_id: i32) -> StoreResult<bool> {
        let self_serve: Option<bool> = sqlx::query_scalar(
            "SELECT COALESCE(plan_source = 'subscription', 0) <> 0 FROM company WHERE id = ? LIMIT 1",
        )
        .bind(company_id)
        .fetch_optional(self.0)
        .await?;
        Ok(self_serve.unwrap_or(false))
    }

    async fn plan_invite_status(
        &self,
        company_id: i32,
        invite_id: &str,
    ) -> StoreResult<Option<String>> {
        sqlx::query_scalar(
            "SELECT CAST(status AS CHAR) FROM plan_invites WHERE id = ? AND company_id = ? LIMIT 1",
        )
        .bind(invite_id)
        .bind(company_id)
        .fetch_optional(self.0)
        .await
    }

    async fn bank_verification_until(
        &self,
        company_id: i32,
    ) -> StoreResult<Option<NaiveDateTime>> {
        bank_verification_until(self.0, company_id).await
    }
}

/// `BillingStore::bank_verification_until` on a pool.
pub async fn bank_verification_until(
    pool: &MySqlPool,
    company_id: i32,
) -> StoreResult<Option<NaiveDateTime>> {
    let until: Option<Option<NaiveDateTime>> = sqlx::query_scalar(
        "SELECT bank_verification_until FROM signup_intents
          WHERE company_id = ? AND status = 'awaiting_bank_verification'
            AND bank_verification_until IS NOT NULL
          LIMIT 1",
    )
    .bind(company_id)
    .fetch_optional(pool)
    .await?;
    Ok(until.flatten())
}
