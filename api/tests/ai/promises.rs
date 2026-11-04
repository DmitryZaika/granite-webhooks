//! AI promises close themselves once the rep does the work in the CRM
//! (`promises::close_finished`): a quote saved, a document sent to sign, an
//! event on the schedule, a call.

use api::promises::close_finished;
use sqlx::MySqlPool;

/// An open task of the seed's rep 100 for customer 1000 (317-555-0100), said
/// on a call at 10:00 on Oct 29, for 3 PM on Oct 30.
async fn task(pool: &MySqlPool, kind: &str, crm_action: Option<&str>) -> u64 {
    sqlx::query(
        "INSERT INTO ai_promises
                (company_id, user_id, scan_id, source, kind, crm_action, promised_at,
                 customer_id, customer_digits, title, said, due_at)
         VALUES (100, 100, 1, 'call', ?, ?, '2026-10-29 10:00:00',
                 1000, '3175550100', 'Task', '', '2026-10-30 15:00:00')",
    )
    .bind(kind)
    .bind(crm_action)
    .execute(pool)
    .await
    .unwrap()
    .last_insert_id()
}

async fn status(pool: &MySqlPool, id: u64) -> String {
    sqlx::query_scalar("SELECT CAST(status AS CHAR) FROM ai_promises WHERE id = ?")
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap()
}

#[test_db::test(seed)]
async fn tasks_close_once_the_rep_does_them_in_the_crm(pool: MySqlPool) {
    sqlx::query("UPDATE users SET ai_promises = 1 WHERE id = 100")
        .execute(&pool)
        .await
        .unwrap();
    let quote = task(&pool, "quote", None).await;
    let contract = task(&pool, "promise", Some("contract")).await;
    let schedule = task(&pool, "promise", Some("schedule")).await;
    let call = task(&pool, "promise", Some("call")).await;
    let other = task(&pool, "promise", None).await;

    // A quote saved before it was said, a draft to sign, an unscheduled
    // event, and the call it was said on: nothing is done yet.
    let quote_id = sqlx::query(
        "INSERT INTO quotes (customer_id, quote_type, created_date, sales_rep, company_id)
         VALUES (1000, 'kitchen', '2026-10-01 09:00:00', '100', 100)",
    )
    .execute(&pool)
    .await
    .unwrap()
    .last_insert_id();
    sqlx::query(
        "INSERT INTO quote_revisions (quote_id, revision_number, drawing_json, created_at)
         VALUES (?, 1, '{}', '2026-10-28 09:00:00')",
    )
    .bind(quote_id)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::raw_sql(
        "INSERT INTO signature_requests
                (company_id, customer_id, created_by_user_id, filename, file_url, signer_name,
                 signer_email, status, created_at)
         VALUES (100, 1000, 100, 'contract.pdf', 'https://x.test/c.pdf', 'Alice', 'alice@x.test',
                 'draft', '2026-10-29 11:00:00');
         INSERT INTO events (title, start_date, end_date, created_user_id, customer_id, unscheduled, created_date)
         VALUES ('Template', '2026-11-02 14:00:00', '2026-11-02 15:00:00', 100, 1000, 1, '2026-10-29 11:00:00');
         INSERT INTO phone_calls (company_id, direction, other_number, other_last10, started_at, duration_secs)
         VALUES (100, 'outbound', '+13175550100', '3175550100', '2026-10-29 10:00:00', 300);",
    )
    .execute(&pool)
    .await
    .unwrap();
    assert_eq!(close_finished(&pool).await.unwrap(), 0);

    // Done in the CRM after it was said: each closes.
    sqlx::query(
        "INSERT INTO quote_revisions (quote_id, revision_number, drawing_json, created_at)
         VALUES (?, 2, '{}', '2026-10-29 12:00:00')",
    )
    .bind(quote_id)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::raw_sql(
        "UPDATE signature_requests SET status = 'sent' WHERE company_id = 100 AND customer_id = 1000;
         INSERT INTO events (title, start_date, end_date, created_user_id, customer_id, unscheduled, created_date)
         VALUES ('Measure', '2026-11-03 14:00:00', '2026-11-03 15:00:00', 100, 1000, 0, '2026-10-29 12:00:00');
         INSERT INTO phone_calls (company_id, direction, other_number, other_last10, started_at, duration_secs)
         VALUES (100, 'inbound', '+13175550100', '3175550100', '2026-10-29 16:00:00', 45);",
    )
    .execute(&pool)
    .await
    .unwrap();
    assert_eq!(close_finished(&pool).await.unwrap(), 4);
    for id in [quote, contract, schedule, call] {
        assert_eq!(status(&pool, id).await, "done", "task {id}");
    }
    // Nothing in the CRM finishes a plain promise: a message that keeps it,
    // or Done.
    assert_eq!(status(&pool, other).await, "open");
}
