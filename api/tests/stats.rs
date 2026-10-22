mod common;

use api::period::{month_start_back, today};
use common::{WithSession, server, sessions};
use serde_json::{Value, json};
use sqlx::MySqlPool;

// Seed: "Employee dashboard" section of local_seed.sql. Rita (100) has deals
// 8000-8010; Mark (101) won 8008 this month; Olga (106, company 101) won 8009.
// Rows dated "this month" are on the 1st, "last month" on the 5th of the
// month before.

const ROUTES: [&str; 5] = [
    "/v1/me/stats/sales",
    "/v1/me/stats/deal-conversion",
    "/v1/me/stats/top-stones",
    "/v1/me/stats/sent-emails",
    "/v1/me/stats/sent-sms",
];

fn month(months_back: u32) -> String {
    month_start_back(today(), months_back)
        .format("%Y-%m")
        .to_string()
}

async fn get(pool: MySqlPool, path: &str, session: &str) -> Value {
    server(pool).get(path).with_session(session).await.json()
}

// ---- GET /v1/me/stats/sales -------------------------------------------------

#[test_db::test(seed)]
async fn sales_count_this_months_deals_of_the_user(pool: MySqlPool) {
    let start = month_start_back(today(), 0).to_string();
    let end = today().to_string();
    assert_eq!(
        get(pool.clone(), "/v1/me/stats/sales", sessions::REP).await,
        json!({
            "start_date": start,
            "end_date": end,
            // 8000 + 8010; 8005 (deleted), 8006 (deleted customer) and 8004
            // (last month) are not counted.
            "won_count": 2,
            "lost_count": 1,
            "won_amount": "2200.00",
            // 8002 + 8003 (no amount); 8007 is in a deleted list.
            "active_deals_count": 2,
            "pipeline_amount": "2500.50",
            "new_deals_count": 2
        })
    );
    assert_eq!(
        get(pool.clone(), "/v1/me/stats/sales", sessions::MANAGER).await,
        json!({
            "start_date": start,
            "end_date": end,
            "won_count": 1,
            "lost_count": 0,
            "won_amount": "3000.00",
            "active_deals_count": 0,
            "pipeline_amount": "0.00",
            "new_deals_count": 0
        })
    );
    // Company 101 sees only its own deal.
    let other = get(pool, "/v1/me/stats/sales", sessions::OTHER_COMPANY_REP).await;
    assert_eq!(other["won_count"], 1);
    assert_eq!(other["won_amount"], "8000.00");
}

#[test_db::test(seed)]
async fn sales_of_a_user_with_no_deals_are_zero(pool: MySqlPool) {
    let stats = get(pool, "/v1/me/stats/sales", sessions::ADMIN).await;
    assert_eq!(stats["won_count"], 0);
    assert_eq!(stats["won_amount"], "0.00");
    assert_eq!(stats["pipeline_amount"], "0.00");
}

// ---- GET /v1/me/stats/deal-conversion ---------------------------------------

#[test_db::test(seed)]
async fn deal_conversion_counts_every_live_deal_by_source(pool: MySqlPool) {
    assert_eq!(
        get(pool.clone(), "/v1/me/stats/deal-conversion", sessions::REP).await,
        json!({
            "total": { "won": 3, "created": 6 },
            "leads": { "won": 2, "created": 2 },
            "walk_in": { "won": 0, "created": 1 },
            "call_in": { "won": 1, "created": 2 }
        })
    );
    assert_eq!(
        get(pool, "/v1/me/stats/deal-conversion", sessions::ADMIN).await,
        json!({
            "total": { "won": 0, "created": 0 },
            "leads": { "won": 0, "created": 0 },
            "walk_in": { "won": 0, "created": 0 },
            "call_in": { "won": 0, "created": 0 }
        })
    );
}

// ---- GET /v1/me/stats/top-stones --------------------------------------------

#[test_db::test(seed)]
async fn top_stones_rank_this_months_full_slabs(pool: MySqlPool) {
    assert_eq!(
        get(pool.clone(), "/v1/me/stats/top-stones", sessions::REP).await,
        json!([])
    );

    // Rita sells this month: 2 slabs of 3001 (55.50 sqft), 2 of 3002 (no
    // size), 1 of 3004 and a 4th stone with 1 slab (3003, 1 sqft: only 3 are
    // listed). Never counted: a remnant, a deleted slab, slabs of a deleted
    // stone (3005), a cancelled sale, last month's sale, Mark's sale.
    sqlx::raw_sql(
        r"
        SET @m0 = CAST(DATE_FORMAT(CURRENT_DATE, '%Y-%m-01') AS DATETIME);
        INSERT INTO sales (id, customer_id, seller_id, company_id, price, status, project_address, sale_date, cancelled_date) VALUES
          (9900, 1000, 100, 100, 1, 'pending', '1 Main St', @m0, NULL),
          (9901, 1000, 100, 100, 1, 'pending', '1 Main St', @m0, @m0),
          (9902, 1000, 100, 100, 1, 'pending', '1 Main St', @m0 - INTERVAL 1 MONTH, NULL),
          (9903, 1000, 101, 100, 1, 'pending', '1 Main St', @m0, NULL);
        INSERT INTO slab_inventory (id, stone_id, bundle, sale_id, parent_id, deleted_at, square_feet) VALUES
          (9900, 3001, 'T1', 9900, NULL, NULL, 30.00),
          (9901, 3001, 'T2', 9900, NULL, NULL, 25.50),
          (9902, 3002, 'T3', 9900, NULL, NULL, NULL),
          (9903, 3002, 'T4', 9900, NULL, NULL, NULL),
          (9904, 3004, 'T5', 9900, NULL, NULL, 10.00),
          (9905, 3003, 'T6', 9900, NULL, NULL, 1.00),
          (9906, 3001, 'T7', 9900, 9900, NULL, 99.00),
          (9911, 3005, 'T12', 9900, NULL, NULL, 99.00),
          (9907, 3003, 'T8', 9900, NULL, '2026-01-01', 99.00),
          (9908, 3003, 'T9', 9901, NULL, NULL, 99.00),
          (9909, 3003, 'T10', 9902, NULL, NULL, 99.00),
          (9910, 3003, 'T11', 9903, NULL, NULL, 99.00);
        ",
    )
    .execute(&pool)
    .await
    .unwrap();

    assert_eq!(
        get(pool, "/v1/me/stats/top-stones", sessions::REP).await,
        json!([
            { "stone_id": 3001, "stone_name": "Calacatta Gold", "slabs_sold": 2, "square_feet": "55.50" },
            // Ties on slabs: more square feet first, then the stone id.
            { "stone_id": 3002, "stone_name": "Sold Out Marble", "slabs_sold": 2, "square_feet": "0.00" },
            // Hidden from the catalog, still sold.
            { "stone_id": 3004, "stone_name": "Hidden Granite", "slabs_sold": 1, "square_feet": "10.00" }
        ])
    );
}

// ---- GET /v1/me/stats/sent-emails -------------------------------------------

#[test_db::test(seed)]
async fn sent_emails_count_the_last_three_months(pool: MySqlPool) {
    // Olga: 8405-8407 this month, 8408 last month.
    assert_eq!(
        get(
            pool.clone(),
            "/v1/me/stats/sent-emails",
            sessions::OTHER_COMPANY_REP
        )
        .await,
        json!([{ "month": month(1), "count": 1 }, { "month": month(0), "count": 3 }])
    );
    // Rita: drafts and unsent rows count here (Remix parity); an email of
    // another company does not, one with no company does, and one from three
    // months ago is outside the window.
    sqlx::raw_sql(
        r"
        SET @m0 = CAST(DATE_FORMAT(CURRENT_DATE, '%Y-%m-01') AS DATETIME);
        INSERT INTO emails (id, subject, body, sent_at, sender_user_id, company_id) VALUES
          (9900, 'x', 'b', @m0, 100, 101),
          (9901, 'x', 'b', @m0, 100, NULL),
          (9902, 'x', 'b', @m0 - INTERVAL 3 MONTH, 100, 100);
        UPDATE emails SET deleted_at = @m0 WHERE id = 8401;
        ",
    )
    .execute(&pool)
    .await
    .unwrap();
    assert_eq!(
        get(pool, "/v1/me/stats/sent-emails", sessions::REP).await,
        json!([{ "month": month(1), "count": 1 }, { "month": month(0), "count": 4 }])
    );
}

// ---- GET /v1/me/stats/sent-sms ----------------------------------------------

#[test_db::test(seed)]
async fn sent_sms_follow_the_company_provider(pool: MySqlPool) {
    // Company 100 has no provider, whatever its tables hold.
    assert_eq!(
        get(pool.clone(), "/v1/me/stats/sent-sms", sessions::REP).await,
        json!({ "provider": null, "months": [] })
    );
    // Company 101 texts through CloudTalk: 8305 this month, 8306 last month.
    assert_eq!(
        get(
            pool.clone(),
            "/v1/me/stats/sent-sms",
            sessions::OTHER_COMPANY_REP
        )
        .await,
        json!({
            "provider": "cloudtalk",
            "months": [{ "month": month(1), "count": 1 }, { "month": month(0), "count": 1 }]
        })
    );

    // RingCentral wins once it is set up too. Texts count when sent by Olga or
    // through her extension; failed, inbound or someone else's do not.
    sqlx::raw_sql(
        r"
        SET @m0 = CAST(DATE_FORMAT(CURRENT_DATE, '%Y-%m-01') AS DATETIME);
        UPDATE company SET ringcentral_client_id = 'id', ringcentral_client_secret = 'secret',
                           ringcentral_jwt = 'jwt' WHERE id = 101;
        UPDATE users SET ringcentral_extension_id = '777' WHERE id = 106;
        INSERT INTO ringcentral_sms (id, recipient, text, direction, status, sender_user_id, agent, company_id, created_date) VALUES
          (9900, 1, 'a', 'outbound', 'sent',     106,  NULL,  101, @m0),
          (9901, 1, 'b', 'outbound', 'sent',     NULL, '777', 101, @m0),
          (9902, 1, 'c', 'outbound', 'failed',   106,  NULL,  101, @m0),
          (9903, 1, 'd', 'inbound',  'received', 106,  NULL,  101, @m0),
          (9904, 1, 'e', 'outbound', 'sent',     NULL, '888', 101, @m0),
          (9905, 1, 'f', 'outbound', 'sent',     106,  NULL,  100, @m0);
        ",
    )
    .execute(&pool)
    .await
    .unwrap();
    assert_eq!(
        get(pool, "/v1/me/stats/sent-sms", sessions::OTHER_COMPANY_REP).await,
        json!({ "provider": "ringcentral", "months": [{ "month": month(0), "count": 2 }] })
    );
}

// ---- auth -------------------------------------------------------------------

#[test_db::test(seed)]
async fn stats_need_an_employee(pool: MySqlPool) {
    let server = server(pool);
    for path in ROUTES {
        server
            .get(path)
            .expect_failure()
            .await
            .assert_status_unauthorized();
        server
            .get(path)
            .with_session(sessions::VIEWER)
            .expect_failure()
            .await
            .assert_status_forbidden();
    }
}
