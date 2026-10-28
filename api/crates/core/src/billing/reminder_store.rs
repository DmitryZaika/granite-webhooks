//! `billing_reminder_emails` and the billing facts the reminders read, on MySQL.

use super::reminder_emails::OnboardingLine;
use super::reminders::{
    ClaimedReminder, CompanyBillingSnapshot, DunningCycle, DunningKind, FIRST_CHARGE_NOTICE_DAYS,
    FIRST_CHARGE_RECHECK_HOURS, NO_USER, RECOVERED_NOTICE_WINDOW_DAYS, REMINDER_CYCLE_MAX_AGE_DAYS,
    REMINDER_MAX_ATTEMPTS, REMINDER_RETRY_AFTER_MINUTES, ReminderContact, ReminderKey, ReminderRow,
    ReminderStatus, ReminderStep, ReminderStore,
};
use super::store::{BillingStore, MySqlBillingStore, StoreResult};
use chrono::{Duration, NaiveDateTime};
use sqlx::{FromRow, MySqlPool};

pub struct MySqlReminderStore<'a>(pub &'a MySqlPool);

#[derive(FromRow)]
struct ReminderDbRow {
    id: i64,
    company_id: i32,
    user_id: i32,
    cycle_kind: String,
    cycle_started_at: NaiveDateTime,
    step: String,
    status: String,
    attempts: i32,
    claimed_at: NaiveDateTime,
}

impl ReminderDbRow {
    /// `None` for a value this code does not know.
    fn parse(self) -> Option<ReminderRow> {
        Some(ReminderRow {
            id: self.id,
            key: ReminderKey {
                company_id: self.company_id,
                user_id: self.user_id,
                cycle: DunningCycle {
                    kind: DunningKind::parse(&self.cycle_kind)?,
                    started_at: self.cycle_started_at,
                },
                step: ReminderStep::parse(&self.step)?,
            },
            status: ReminderStatus::parse(&self.status)?,
            attempts: self.attempts,
            claimed_at: self.claimed_at,
        })
    }
}

const REMINDER_SELECT: &str = "SELECT id, company_id, user_id, cycle_kind, cycle_started_at, step,
        status, attempts, claimed_at
   FROM billing_reminder_emails";

const KEY_WHERE: &str = "company_id = ? AND user_id = ? AND cycle_kind = ?
        AND cycle_started_at = ? AND step = ?";

fn is_duplicate(err: &sqlx::Error) -> bool {
    err.as_database_error()
        .is_some_and(sqlx::error::DatabaseError::is_unique_violation)
}

impl MySqlReminderStore<'_> {
    async fn set_status(
        &self,
        row: ClaimedReminder,
        status: ReminderStatus,
        error_code: Option<&str>,
        message_id: Option<&str>,
        sent_at: Option<NaiveDateTime>,
    ) -> StoreResult<()> {
        sqlx::query(
            "UPDATE billing_reminder_emails
                SET status = ?, error_code = ?, ses_message_id = COALESCE(?, ses_message_id),
                    sent_at = COALESCE(?, sent_at)
              WHERE id = ? AND company_id = ? AND status = 'sending'",
        )
        .bind(status.as_str())
        .bind(error_code)
        .bind(message_id)
        .bind(sent_at)
        .bind(row.id)
        .bind(row.company_id)
        .execute(self.0)
        .await?;
        Ok(())
    }
}

impl ReminderStore for MySqlReminderStore<'_> {
    async fn candidate_company_ids(&self, now: NaiveDateTime) -> StoreResult<Vec<i32>> {
        let fresh = now - Duration::days(REMINDER_CYCLE_MAX_AGE_DAYS);
        let recovered = now - Duration::days(RECOVERED_NOTICE_WINDOW_DAYS);
        // A failed payment in its cycle or recently recovered; a company asked
        // to pay that has not subscribed; one that subscribed after being
        // emailed.
        sqlx::query_scalar(
            "SELECT c.id
               FROM company c
               LEFT JOIN company_billing cb ON cb.company_id = c.id
              WHERE c.billing_required = 1 AND c.deleted_at IS NULL
                AND (
                  (cb.payment_status IN ('past_due', 'suspended') AND cb.failed_payment_at > ?)
                  OR (cb.payment_status = 'active' AND cb.recovered_at > ?)
                  OR (c.billing_required_at > ?
                      AND (cb.company_id IS NULL OR cb.payment_status = 'none'))
                  OR (c.billing_required_at > ? AND cb.payment_status = 'active'
                      AND EXISTS (
                        SELECT 1 FROM billing_reminder_emails r
                         WHERE r.company_id = c.id AND r.cycle_kind = 'first_payment'
                           AND r.cycle_started_at = c.billing_required_at
                           AND r.step <> 'resolved' AND r.status IN ('sent', 'unknown')))
                )
              ORDER BY c.id
              LIMIT 500",
        )
        .bind(fresh)
        .bind(recovered)
        .bind(fresh)
        .bind(fresh)
        .fetch_all(self.0)
        .await
    }

    async fn first_charge_company_ids(&self, now: NaiveDateTime) -> StoreResult<Vec<i32>> {
        // Stripe charges whether or not billing is required, so no filter on it.
        sqlx::query_scalar(
            "SELECT cb.company_id
               FROM company_billing cb
               JOIN company c ON c.id = cb.company_id
              WHERE c.deleted_at IS NULL AND cb.subscription_status = 'trialing'
                AND cb.trial_end_at > ? AND cb.trial_end_at <= ?
                AND (cb.cancel_at IS NULL OR cb.cancel_at > cb.trial_end_at)
                AND NOT EXISTS (
                  SELECT 1 FROM billing_reminder_emails r
                   WHERE r.company_id = cb.company_id AND r.cycle_kind = 'first_charge'
                     AND r.cycle_started_at = cb.trial_end_at AND r.status = 'skipped'
                     AND r.claimed_at > ?)
              ORDER BY cb.trial_end_at, cb.company_id
              LIMIT 500",
        )
        .bind(now)
        .bind(now + Duration::days(FIRST_CHARGE_NOTICE_DAYS))
        .bind(now - Duration::hours(FIRST_CHARGE_RECHECK_HOURS))
        .fetch_all(self.0)
        .await
    }

    async fn snapshot(&self, company_id: i32) -> StoreResult<CompanyBillingSnapshot> {
        let company: Option<(bool, Option<NaiveDateTime>)> = sqlx::query_as(
            "SELECT billing_required <> 0, billing_required_at
               FROM company WHERE id = ? AND deleted_at IS NULL LIMIT 1",
        )
        .bind(company_id)
        .fetch_optional(self.0)
        .await?;
        let (required, required_since) = company.unwrap_or((false, None));
        Ok(CompanyBillingSnapshot {
            required,
            required_since,
            record: MySqlBillingStore(self.0).by_company(company_id).await?,
            awaiting_bank: super::store::bank_verification_until(self.0, company_id)
                .await?
                .is_some(),
        })
    }

    async fn managers(&self, company_id: i32) -> StoreResult<Vec<i32>> {
        MySqlBillingStore(self.0).managers(company_id).await
    }

    async fn contacts(
        &self,
        company_id: i32,
        user_ids: &[i32],
    ) -> StoreResult<Vec<ReminderContact>> {
        if user_ids.is_empty() {
            return Ok(Vec::new());
        }
        let mut query = sqlx::QueryBuilder::new(
            "SELECT id, name, TRIM(email) FROM users
              WHERE is_deleted = 0 AND email IS NOT NULL AND TRIM(email) <> '' AND company_id = ",
        );
        query.push_bind(company_id).push(" AND id IN (");
        let mut ids = query.separated(", ");
        for id in user_ids {
            ids.push_bind(*id);
        }
        query.push(") ORDER BY id");
        let rows: Vec<(i32, Option<String>, String)> =
            query.build_query_as().fetch_all(self.0).await?;
        Ok(rows
            .into_iter()
            .map(|(user_id, name, email)| ReminderContact {
                user_id,
                name,
                email,
            })
            .collect())
    }

    async fn company_name(&self, company_id: i32) -> StoreResult<String> {
        let name: Option<Option<String>> =
            sqlx::query_scalar("SELECT name FROM company WHERE id = ? LIMIT 1")
                .bind(company_id)
                .fetch_optional(self.0)
                .await?;
        Ok(name.flatten().unwrap_or_default())
    }

    async fn onboarding_line(&self, company_id: i32) -> StoreResult<Option<OnboardingLine>> {
        Ok(crate::onboarding_call::store::load(self.0, company_id)
            .await?
            .and_then(|stored| OnboardingLine::of(&stored.commitment)))
    }

    async fn reminders(
        &self,
        company_id: i32,
        cycle: DunningCycle,
    ) -> StoreResult<Vec<ReminderRow>> {
        let rows: Vec<ReminderDbRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "{REMINDER_SELECT}
              WHERE company_id = ? AND cycle_kind = ? AND cycle_started_at = ?
              ORDER BY id"
        )))
        .bind(company_id)
        .bind(cycle.kind.as_str())
        .bind(cycle.started_at)
        .fetch_all(self.0)
        .await?;
        Ok(rows.into_iter().filter_map(ReminderDbRow::parse).collect())
    }

    async fn claim(&self, key: &ReminderKey, now: NaiveDateTime) -> StoreResult<Option<i64>> {
        let inserted = sqlx::query(
            "INSERT INTO billing_reminder_emails
               (company_id, user_id, cycle_kind, cycle_started_at, step, status, attempts, claimed_at)
             VALUES (?, ?, ?, ?, ?, 'sending', 1, ?)",
        )
        .bind(key.company_id)
        .bind(key.user_id)
        .bind(key.cycle.kind.as_str())
        .bind(key.cycle.started_at)
        .bind(key.step.as_str())
        .bind(now)
        .execute(self.0)
        .await;
        match inserted {
            Ok(done) => return Ok(i64::try_from(done.last_insert_id()).ok()),
            Err(err) if is_duplicate(&err) => {}
            Err(err) => return Err(err),
        }
        // Only a rejected send with attempts left, last tried a while ago, may
        // be taken again.
        let retried = sqlx::query(sqlx::AssertSqlSafe(format!(
            "UPDATE billing_reminder_emails
                SET status = 'sending', attempts = attempts + 1, claimed_at = ?, error_code = NULL
              WHERE {KEY_WHERE} AND status = 'failed' AND attempts < ? AND claimed_at <= ?"
        )))
        .bind(now)
        .bind(key.company_id)
        .bind(key.user_id)
        .bind(key.cycle.kind.as_str())
        .bind(key.cycle.started_at)
        .bind(key.step.as_str())
        .bind(REMINDER_MAX_ATTEMPTS)
        .bind(now - Duration::minutes(REMINDER_RETRY_AFTER_MINUTES))
        .execute(self.0)
        .await?;
        if retried.rows_affected() != 1 {
            return Ok(None);
        }
        sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "SELECT id FROM billing_reminder_emails WHERE {KEY_WHERE}"
        )))
        .bind(key.company_id)
        .bind(key.user_id)
        .bind(key.cycle.kind.as_str())
        .bind(key.cycle.started_at)
        .bind(key.step.as_str())
        .fetch_optional(self.0)
        .await
    }

    async fn skip_cycle(
        &self,
        company_id: i32,
        cycle: DunningCycle,
        step: ReminderStep,
        now: NaiveDateTime,
    ) -> StoreResult<()> {
        sqlx::query(
            "INSERT INTO billing_reminder_emails
               (company_id, user_id, cycle_kind, cycle_started_at, step, status, attempts, claimed_at)
             VALUES (?, ?, ?, ?, ?, 'skipped', 0, ?)
             ON DUPLICATE KEY UPDATE claimed_at = VALUES(claimed_at)",
        )
        .bind(company_id)
        .bind(NO_USER)
        .bind(cycle.kind.as_str())
        .bind(cycle.started_at)
        .bind(step.as_str())
        .bind(now)
        .execute(self.0)
        .await?;
        Ok(())
    }

    async fn mark_sent(
        &self,
        row: ClaimedReminder,
        message_id: Option<&str>,
        now: NaiveDateTime,
    ) -> StoreResult<()> {
        self.set_status(row, ReminderStatus::Sent, None, message_id, Some(now))
            .await
    }

    async fn mark_failed(&self, row: ClaimedReminder, error_code: &str) -> StoreResult<()> {
        self.set_status(row, ReminderStatus::Failed, Some(error_code), None, None)
            .await
    }

    async fn mark_unknown(&self, row: ClaimedReminder, error_code: &str) -> StoreResult<()> {
        self.set_status(row, ReminderStatus::Unknown, Some(error_code), None, None)
            .await
    }

    async fn sweep_stuck(&self, cutoff: NaiveDateTime) -> StoreResult<Vec<ReminderRow>> {
        let stuck: Vec<ReminderDbRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "{REMINDER_SELECT} WHERE status = 'sending' AND claimed_at < ? ORDER BY id LIMIT 100"
        )))
        .bind(cutoff)
        .fetch_all(self.0)
        .await?;
        let mut swept = Vec::new();
        for row in stuck {
            let (id, company_id) = (row.id, row.company_id);
            let Some(mut parsed) = row.parse() else {
                continue;
            };
            let updated = sqlx::query(
                "UPDATE billing_reminder_emails
                    SET status = 'unknown', error_code = 'stuck_sending'
                  WHERE id = ? AND company_id = ? AND status = 'sending'",
            )
            .bind(id)
            .bind(company_id)
            .execute(self.0)
            .await?;
            if updated.rows_affected() == 1 {
                parsed.status = ReminderStatus::Unknown;
                swept.push(parsed);
            }
        }
        Ok(swept)
    }
}
