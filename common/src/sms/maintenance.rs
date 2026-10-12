//! Upkeep of one company's SMS table, run (throttled) before the inbox is read.
//!
//! A send that never finished is failed after [`PENDING_TIMEOUT_MINUTES`], and
//! the inbound echoes it was hiding come back; then the races the insert path
//! cannot see are closed over the last [`RECONCILE_WINDOW_MINUTES`].
//!
//! Port of `runSmsMaintenance` and `reconcileSmsDerivedFields` in the CRM.

use chrono::{DateTime, Utc};
use sqlx::MySqlPool;
use std::collections::HashMap;

use super::derived::{
    EchoCandidate, SMS_ECHO_WINDOW_SECONDS, SmsProvider, company_sender_phones,
    find_echoed_outbound,
};

/// An outbound row still `pending` after this long is failed.
pub const PENDING_TIMEOUT_MINUTES: i64 = 5;

/// How far back the reconcile looks; both sides of every statement are
/// bounded by it, so it stays cheap.
pub const RECONCILE_WINDOW_MINUTES: i64 = 60;

fn in_list(count: usize) -> String {
    vec!["?"; count].join(", ")
}

/// Fails stale sends, un-hides their echoes, then reconciles. The sweep runs
/// first: the reconcile's un-hide pass only looks an hour back.
pub async fn run(
    pool: &MySqlPool,
    provider: SmsProvider,
    company_id: i32,
) -> Result<(), sqlx::Error> {
    fail_stale_pending(pool, provider, company_id).await?;
    reconcile(pool, provider, company_id).await
}

async fn fail_stale_pending(
    pool: &MySqlPool,
    provider: SmsProvider,
    company_id: i32,
) -> Result<(), sqlx::Error> {
    let table = provider.table();
    let ids: Vec<i32> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT id FROM {table}
          WHERE company_id = ? AND direction = 'outbound' AND status = 'pending'
            AND created_date < NOW() - INTERVAL {PENDING_TIMEOUT_MINUTES} MINUTE"
    )))
    .bind(company_id)
    .fetch_all(pool)
    .await?;
    if ids.is_empty() {
        return Ok(());
    }
    let mut update = sqlx::query(sqlx::AssertSqlSafe(format!(
        "UPDATE {table} SET status = 'failed', error_message = 'pending_timeout'
          WHERE company_id = ? AND id IN ({})",
        in_list(ids.len())
    )))
    .bind(company_id);
    for id in &ids {
        update = update.bind(id);
    }
    update.execute(pool).await?;
    for id in ids {
        clear_echoes_of(pool, provider, company_id, i64::from(id)).await?;
    }
    Ok(())
}

#[derive(sqlx::FromRow)]
struct HiddenEcho {
    id: i32,
    text: String,
    sender10: Option<String>,
    recipient10: Option<String>,
    created_date: DateTime<Utc>,
}

/// A send failed, so the inbound rows hidden as its echoes show again — unless
/// another live send still matches one, which then hides it instead.
pub async fn clear_echoes_of(
    pool: &MySqlPool,
    provider: SmsProvider,
    company_id: i32,
    sms_id: i64,
) -> Result<(), sqlx::Error> {
    let table = provider.table();
    let hidden: Vec<HiddenEcho> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT id, text, sender10, recipient10, created_date FROM {table}
          WHERE company_id = ? AND is_echo = 1 AND echo_of_sms_id = ?"
    )))
    .bind(company_id)
    .bind(sms_id)
    .fetch_all(pool)
    .await?;
    for row in hidden {
        let candidate = EchoCandidate {
            company_id,
            text: &row.text,
            sender10: row.sender10.as_deref(),
            recipient10: row.recipient10.as_deref().unwrap_or_default(),
            created_date: row.created_date,
        };
        let replacement = find_echoed_outbound(pool, provider, &candidate, Some(sms_id)).await?;
        let sql = if replacement.is_some() {
            format!("UPDATE {table} SET echo_of_sms_id = ? WHERE company_id = ? AND id = ?")
        } else {
            format!(
                "UPDATE {table} SET is_echo = 0, echo_of_sms_id = ?
                  WHERE company_id = ? AND id = ?"
            )
        };
        sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(replacement)
            .bind(company_id)
            .bind(row.id)
            .execute(pool)
            .await?;
    }
    Ok(())
}

#[derive(sqlx::FromRow)]
struct EchoPair {
    inbound_id: i32,
    outbound_id: i32,
    gap: i64,
}

/// Per inbound row, the closest outbound (then the lowest id) — the insert
/// path's tie-break.
fn closest_outbound(pairs: &[EchoPair]) -> HashMap<i32, i32> {
    let mut best: HashMap<i32, (i64, i32)> = HashMap::new();
    for pair in pairs {
        let candidate = (pair.gap, pair.outbound_id);
        best.entry(pair.inbound_id)
            .and_modify(|current| {
                if candidate < *current {
                    *current = candidate;
                }
            })
            .or_insert(candidate);
    }
    best.into_iter()
        .map(|(inbound, (_, outbound))| (inbound, outbound))
        .collect()
}

async fn reconcile(
    pool: &MySqlPool,
    provider: SmsProvider,
    company_id: i32,
) -> Result<(), sqlx::Error> {
    let table = provider.table();
    let window = RECONCILE_WINDOW_MINUTES;

    // (a) An inbound echo that arrived while its outbound was still being
    // inserted saw no match, and the outbound's reverse pass ran before the
    // inbound existed. Both sides carry their own constant date range so MySQL
    // joins two small sets.
    let pairs: Vec<EchoPair> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT inb.id AS inbound_id, o.id AS outbound_id,
                ABS(TIMESTAMPDIFF(SECOND, o.created_date, inb.created_date)) AS gap
           FROM {table} inb
           JOIN {table} o
             ON o.company_id = ?
            AND o.direction = 'outbound'
            AND o.status IN ('sent', 'pending')
            AND o.created_date BETWEEN NOW() - INTERVAL {} MINUTE AND NOW() + INTERVAL 5 MINUTE
            AND o.text = inb.text
            AND o.created_date
                BETWEEN DATE_SUB(inb.created_date, INTERVAL {SMS_ECHO_WINDOW_SECONDS} SECOND)
                    AND DATE_ADD(inb.created_date, INTERVAL {SMS_ECHO_WINDOW_SECONDS} SECOND)
            AND (o.recipient10 IN (inb.sender10, inb.recipient10)
                 OR o.sender10 IN (inb.sender10, inb.recipient10))
          WHERE inb.company_id = ?
            AND inb.direction = 'inbound'
            AND inb.agent IS NULL
            AND inb.is_echo = 0
            AND inb.text <> ''
            AND inb.created_date >= NOW() - INTERVAL {window} MINUTE",
        window + 5
    )))
    .bind(company_id)
    .bind(company_id)
    .fetch_all(pool)
    .await?;
    for (inbound_id, outbound_id) in closest_outbound(&pairs) {
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "UPDATE {table} SET is_echo = 1, echo_of_sms_id = ? WHERE company_id = ? AND id = ?"
        )))
        .bind(outbound_id)
        .bind(company_id)
        .bind(inbound_id)
        .execute(pool)
        .await?;
    }

    // (b) Echoes whose outbound has failed since. Recent rows only: an older
    // failure was reconciled by the pass that saw it.
    let failed: Vec<i32> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT DISTINCT o.id
           FROM {table} r
           JOIN {table} o ON o.id = r.echo_of_sms_id AND o.company_id = r.company_id
          WHERE r.company_id = ? AND r.is_echo = 1 AND r.echo_of_sms_id IS NOT NULL
            AND r.created_date >= NOW() - INTERVAL 1 HOUR
            AND o.status = 'failed'"
    )))
    .bind(company_id)
    .fetch_all(pool)
    .await?;
    for id in failed {
        clear_echoes_of(pool, provider, company_id, i64::from(id)).await?;
    }

    // (c) Rows a racing writer left keyed under a company line before the
    // line's second piece of evidence was seen.
    let senders: Vec<String> = company_sender_phones(pool, provider, company_id)
        .await?
        .into_iter()
        .collect();
    if senders.is_empty() {
        return Ok(());
    }
    let mut rekey = sqlx::query(sqlx::AssertSqlSafe(format!(
        "UPDATE {table} SET phone_digits = recipient10
          WHERE company_id = ?
            AND created_date >= NOW() - INTERVAL {window} MINUTE
            AND direction = 'inbound'
            AND sender10 IN ({})
            AND phone_digits <> recipient10",
        in_list(senders.len())
    )))
    .bind(company_id);
    for sender in &senders {
        rekey = rekey.bind(sender);
    }
    rekey.execute(pool).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{EchoPair, closest_outbound, run};
    use crate::sms::derived::SmsProvider;
    use sqlx::MySqlPool;

    const COMPANY: i32 = 42;
    const CUSTOMER: &str = "3173161456";
    const LINE: &str = "6468956758";

    #[test]
    fn the_closest_outbound_then_the_lowest_id_wins() {
        let pair = |inbound_id, outbound_id, gap| EchoPair {
            inbound_id,
            outbound_id,
            gap,
        };
        let chosen = closest_outbound(&[
            pair(1, 10, 30),
            pair(1, 11, 5),
            pair(1, 9, 5),
            pair(2, 12, 1),
        ]);
        assert_eq!(chosen[&1], 9);
        assert_eq!(chosen[&2], 12);
    }

    /// A `cloudtalk_sms` row `seconds_ago` old, with its derived columns:
    /// outbound without a sender, inbound with one.
    async fn row(
        pool: &MySqlPool,
        status: &str,
        sender10: Option<&str>,
        recipient10: &str,
        phone_digits: &str,
        text: &str,
        seconds_ago: i64,
    ) -> i32 {
        let result = sqlx::query(
            "INSERT INTO cloudtalk_sms (company_id, sender, recipient, text, direction, status, created_date,
                                        sender10, recipient10, phone_digits)
             VALUES (?, ?, ?, ?, ?, ?, NOW() - INTERVAL ? SECOND, ?, ?, ?)",
        )
        .bind(COMPANY)
        .bind(sender10)
        .bind(recipient10)
        .bind(text)
        .bind(if sender10.is_some() { "inbound" } else { "outbound" })
        .bind(status)
        .bind(seconds_ago)
        .bind(sender10)
        .bind(recipient10)
        .bind(phone_digits)
        .execute(pool)
        .await
        .unwrap();
        i32::try_from(result.last_insert_id()).unwrap()
    }

    async fn echo_of(pool: &MySqlPool, id: i32) -> (i8, Option<i32>, String) {
        sqlx::query_as(
            "SELECT is_echo, echo_of_sms_id, phone_digits FROM cloudtalk_sms WHERE id = ?",
        )
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap()
    }

    #[test_db::test]
    async fn marks_an_echo_both_writers_raced_past(pool: MySqlPool) {
        let outbound = row(&pool, "sent", None, CUSTOMER, CUSTOMER, "Raced", 60).await;
        let inbound = row(
            &pool,
            "received",
            Some(LINE),
            CUSTOMER,
            CUSTOMER,
            "Raced",
            55,
        )
        .await;
        run(&pool, SmsProvider::Cloudtalk, COMPANY).await.unwrap();
        let (is_echo, of, _) = echo_of(&pool, inbound).await;
        assert_eq!((is_echo, of), (1, Some(outbound)));
    }

    #[test_db::test]
    async fn marks_an_echo_up_to_an_hour_old_against_the_closest_send(pool: MySqlPool) {
        let text = "Missed by the old window";
        row(&pool, "sent", None, CUSTOMER, CUSTOMER, text, 3000).await;
        let near = row(&pool, "sent", None, CUSTOMER, CUSTOMER, text, 2900).await;
        let inbound = row(
            &pool,
            "received",
            Some(LINE),
            CUSTOMER,
            CUSTOMER,
            text,
            2890,
        )
        .await;
        run(&pool, SmsProvider::Cloudtalk, COMPANY).await.unwrap();
        assert_eq!(echo_of(&pool, inbound).await.1, Some(near));
    }

    #[test_db::test]
    async fn un_hides_the_echo_of_a_failed_send(pool: MySqlPool) {
        let outbound = row(&pool, "failed", None, CUSTOMER, CUSTOMER, "Expired", 400).await;
        let inbound = row(
            &pool,
            "received",
            Some(LINE),
            CUSTOMER,
            CUSTOMER,
            "Expired",
            395,
        )
        .await;
        sqlx::query("UPDATE cloudtalk_sms SET is_echo = 1, echo_of_sms_id = ? WHERE id = ?")
            .bind(outbound)
            .bind(inbound)
            .execute(&pool)
            .await
            .unwrap();
        run(&pool, SmsProvider::Cloudtalk, COMPANY).await.unwrap();
        let (is_echo, of, _) = echo_of(&pool, inbound).await;
        assert_eq!((is_echo, of), (0, None));
    }

    #[test_db::test]
    async fn fails_a_stuck_send_and_shows_its_echo(pool: MySqlPool) {
        let outbound = row(&pool, "pending", None, CUSTOMER, CUSTOMER, "Stuck", 600).await;
        let inbound = row(
            &pool,
            "received",
            Some(LINE),
            CUSTOMER,
            CUSTOMER,
            "Stuck",
            598,
        )
        .await;
        sqlx::query("UPDATE cloudtalk_sms SET is_echo = 1, echo_of_sms_id = ? WHERE id = ?")
            .bind(outbound)
            .bind(inbound)
            .execute(&pool)
            .await
            .unwrap();
        run(&pool, SmsProvider::Cloudtalk, COMPANY).await.unwrap();
        let status: String =
            sqlx::query_scalar("SELECT CAST(status AS CHAR) FROM cloudtalk_sms WHERE id = ?")
                .bind(outbound)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(status, "failed");
        assert_eq!(echo_of(&pool, inbound).await.0, 0);
    }

    #[test_db::test]
    async fn rekeys_a_row_left_under_a_company_line(pool: MySqlPool) {
        sqlx::query(
            "INSERT INTO sms_company_phones (provider, company_id, phone10, agent_sender, inbound_recipient)
             VALUES ('cloudtalk', ?, ?, 1, 1)",
        )
        .bind(COMPANY)
        .bind(LINE)
        .execute(&pool)
        .await
        .unwrap();
        let stale = row(
            &pool,
            "received",
            Some(LINE),
            CUSTOMER,
            LINE,
            "provider echo",
            100,
        )
        .await;
        run(&pool, SmsProvider::Cloudtalk, COMPANY).await.unwrap();
        assert_eq!(echo_of(&pool, stale).await.2, CUSTOMER);
    }
}
