mod common;

use axum::http::StatusCode;
use common::{WithSession, ids, server, sessions};
use serde_json::{Value, json};
use sqlx::MySqlPool;

// Seed: Dina (107) owns deals 8600-8607 (see the "Deals board" block of
// local_seed.sql). Groups 100 'Sales', 104 'Remodel', 105 hidden; group 1 is
// the shared default from the migrations.

async fn get(pool: &MySqlPool, path: &str, session: &str) -> Value {
    let res = server(pool.clone()).get(path).with_session(session).await;
    res.assert_status_ok();
    res.json()
}

async fn scalar_i64(pool: &MySqlPool, sql: &str) -> i64 {
    sqlx::query_scalar::<_, i64>(sqlx::AssertSqlSafe(sql.to_string()))
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn rows(pool: &MySqlPool, sql: &str) -> Vec<Vec<String>> {
    use sqlx::Row;
    sqlx::query(sqlx::AssertSqlSafe(sql.to_string()))
        .fetch_all(pool)
        .await
        .unwrap()
        .into_iter()
        .map(|row| {
            (0..row.len())
                .map(|i| {
                    row.try_get::<Option<String>, _>(i)
                        .ok()
                        .flatten()
                        .or_else(|| {
                            row.try_get::<Option<i64>, _>(i)
                                .ok()
                                .flatten()
                                .map(|v| v.to_string())
                        })
                        .unwrap_or_else(|| "NULL".into())
                })
                .collect()
        })
        .collect()
}

// ---- groups and stages ------------------------------------------------------

#[test_db::test(seed)]
async fn groups_flag_overdue_activities_of_the_callers_deals(pool: MySqlPool) {
    let groups = get(&pool, "/v1/deal-groups", sessions::DEALER).await;
    let names: Vec<(i64, &str, bool)> = groups
        .as_array()
        .unwrap()
        .iter()
        .map(|g| {
            (
                g["id"].as_i64().unwrap(),
                g["name"].as_str().unwrap(),
                g["has_overdue_activity"].as_bool().unwrap(),
            )
        })
        .collect();
    // Hidden group 105 and company 101's group are not listed. 8600 has an
    // overdue activity in group 100.
    assert!(names.contains(&(100, "Sales", true)));
    assert!(names.contains(&(104, "Remodel", false)));
    assert!(names.iter().all(|(id, _, _)| *id != 105 && *id != 101));
    assert_eq!(names[0].0, 1, "shared group first (id order)");

    // Won deal 8603 has an overdue activity too.
    let won = get(&pool, "/v1/deal-groups?status=won", sessions::DEALER).await;
    let sales = won
        .as_array()
        .unwrap()
        .iter()
        .find(|g| g["id"] == 100)
        .unwrap();
    assert_eq!(sales["has_overdue_activity"], true);
    let lost = get(&pool, "/v1/deal-groups?status=lost", sessions::DEALER).await;
    let sales = lost
        .as_array()
        .unwrap()
        .iter()
        .find(|g| g["id"] == 100)
        .unwrap();
    assert_eq!(sales["has_overdue_activity"], false);
    // Another rep's view of the same group: none of their deals are overdue.
    let rep = get(&pool, "/v1/deal-groups", sessions::REP).await;
    let sales = rep
        .as_array()
        .unwrap()
        .iter()
        .find(|g| g["id"] == 100)
        .unwrap();
    assert_eq!(sales["has_overdue_activity"], false);
}

#[test_db::test(seed)]
async fn stages_of_a_group_in_board_order(pool: MySqlPool) {
    assert_eq!(
        get(&pool, "/v1/deal-lists?group_id=104", sessions::DEALER).await,
        json!([
            { "id": 104, "name": "Measure", "position": 0, "group_id": 104, "group_name": "Remodel" },
            { "id": 105, "name": "Install", "position": 1, "group_id": 104, "group_name": "Remodel" }
        ])
    );
    // Deleted stage 102 is left out.
    assert_eq!(
        ids(&get(&pool, "/v1/deal-lists?group_id=100", sessions::DEALER).await),
        vec![100, 101]
    );
    // Another company's group has no stages for us.
    assert_eq!(
        get(&pool, "/v1/deal-lists?group_id=101", sessions::DEALER).await,
        json!([])
    );
}

#[test_db::test(seed)]
async fn all_stages_by_group_name(pool: MySqlPool) {
    let lists = get(&pool, "/v1/deal-lists", sessions::DEALER).await;
    let ours: Vec<i64> = ids(&lists).into_iter().filter(|id| *id >= 100).collect();
    // Remodel before Sales; hidden group's stage 106 and company 101's 103 absent.
    assert_eq!(ours, vec![104, 105, 100, 101]);
}

// ---- board -----------------------------------------------------------------

#[test_db::test(seed)]
async fn board_lists_the_callers_open_deals_of_the_group(pool: MySqlPool) {
    let board = get(&pool, "/v1/deals?group_id=100", sessions::DEALER).await;
    // 8605 deleted, 8603/8604 closed, 8607 another group; 8606's customer is
    // deleted: the card stays, without a customer.
    assert_eq!(ids(&board), vec![8600, 8601, 8606, 8602]);
    assert_eq!(
        board[0],
        json!({
            "id": 8600,
            "title": "Dina kitchen",
            "amount": "1500.25",
            "list_id": 100,
            "position": 0,
            "status": "New",
            "outcome": "open",
            "lost_reason": null,
            "due_date": "2026-09-30",
            "customer_id": 8695,
            "customer": { "id": 8695, "name": "Nina Lead", "company_name": null, "temperature": "hot" },
            "has_images": true,
            "has_emails": true,
            "has_notes": true,
            "open_activity_count": 2,
            "earliest_activity_deadline": "2026-01-05T09:30:00",
            "next_activity": { "id": 8620, "name": "Call back", "deadline": "2026-01-05T09:30:00", "priority": "high" },
            "sms_flow_phone": null,
            "sales_rep": "Dina Dealer"
        })
    );
    // Undated activity only; completed and deleted ones do not count.
    // Alice's other deals have emails through her address.
    assert_eq!(
        board[1],
        json!({
            "id": 8601,
            "title": null,
            "amount": null,
            "list_id": 100,
            "position": 1,
            "status": "New",
            "outcome": "open",
            "lost_reason": null,
            "due_date": null,
            "customer_id": 1004,
            "customer": { "id": 1004, "name": "Erin Builder", "company_name": "Erin Builders LLC", "temperature": "medium" },
            "has_images": false,
            "has_emails": false,
            "has_notes": false,
            "open_activity_count": 1,
            "earliest_activity_deadline": null,
            "next_activity": { "id": 8622, "name": "Measure", "deadline": null, "priority": "medium" },
            "sms_flow_phone": null,
            "sales_rep": "Dina Dealer"
        })
    );
    assert_eq!(board[2]["customer"], Value::Null);
    // A deleted note does not count.
    assert_eq!(board[3]["has_notes"], false);
}

#[test_db::test(seed)]
async fn admins_see_the_whole_company_board(pool: MySqlPool) {
    let company = get(
        &pool,
        "/v1/deals?group_id=100&scope=company",
        sessions::ADMIN,
    )
    .await;
    let all = ids(&company);
    for id in [8600, 8601, 8606, 8602] {
        assert!(all.contains(&id), "{id} in {all:?}");
    }
    let dina = get(
        &pool,
        "/v1/deals?group_id=100&scope=company&sales_rep=Dina%20Dealer",
        sessions::ADMIN,
    )
    .await;
    assert_eq!(ids(&dina), vec![8600, 8601, 8606, 8602]);
    assert!(
        company
            .as_array()
            .unwrap()
            .iter()
            .all(|deal| deal["sales_rep"].is_string() || deal["sales_rep"].is_null())
    );
    let everyone = get(
        &pool,
        "/v1/deals?group_id=100&scope=company&sales_rep=All",
        sessions::ADMIN,
    )
    .await;
    assert_eq!(ids(&everyone), all);
    server(pool)
        .get("/v1/deals?group_id=100&scope=company")
        .with_session(sessions::DEALER)
        .await
        .assert_status(StatusCode::FORBIDDEN);
}

#[test_db::test(seed)]
async fn board_by_outcome(pool: MySqlPool) {
    let won = get(&pool, "/v1/deals?group_id=100&status=won", sessions::DEALER).await;
    assert_eq!(ids(&won), vec![8603]);
    assert_eq!(won[0]["outcome"], "won");
    let lost = get(
        &pool,
        "/v1/deals?group_id=100&status=lost",
        sessions::DEALER,
    )
    .await;
    assert_eq!(ids(&lost), vec![8604]);
    assert_eq!(lost[0]["lost_reason"], "Too pricey");
    assert_eq!(
        ids(&get(&pool, "/v1/deals?group_id=104", sessions::DEALER).await),
        vec![8607]
    );
}

#[test_db::test(seed)]
async fn board_shows_the_sms_flow_phone(pool: MySqlPool) {
    sqlx::raw_sql(
        "INSERT INTO sms_flows (id, company_id, name, created_at) VALUES (8680, 100, 'Flow', NOW());
         INSERT INTO sms_flow_enrollments (flow_id, company_id, customer_phone_digits, user_id, status, anchor_at)
           VALUES (8680, 100, 3175550105, 107, 'active', NOW());",
    )
    .execute(&pool)
    .await
    .unwrap();
    let board = get(&pool, "/v1/deals?group_id=100", sessions::DEALER).await;
    // Erin's phone is 317-555-0105.
    assert_eq!(board[1]["sms_flow_phone"], "3175550105");
    assert_eq!(board[0]["sms_flow_phone"], Value::Null);
}

#[test_db::test(seed)]
async fn board_is_per_user_and_company(pool: MySqlPool) {
    assert_eq!(
        ids(&get(&pool, "/v1/deals?group_id=100", sessions::OTHER_COMPANY_REP).await),
        Vec::<i64>::new()
    );
    assert_eq!(
        ids(&get(&pool, "/v1/deals?group_id=103", sessions::OTHER_COMPANY_REP).await),
        Vec::<i64>::new(),
        "group 103 is not a group id"
    );
    assert_eq!(
        ids(&get(
            &pool,
            "/v1/deals?group_id=101&status=won",
            sessions::OTHER_COMPANY_REP
        )
        .await),
        vec![8009]
    );
}

#[test_db::test(seed)]
async fn board_rejects_bad_params_and_sessions(pool: MySqlPool) {
    let app = server(pool);
    app.get("/v1/deals")
        .with_session(sessions::DEALER)
        .await
        .assert_status_bad_request();
    app.get("/v1/deals?group_id=x")
        .with_session(sessions::DEALER)
        .await
        .assert_status_bad_request();
    app.get("/v1/deals?group_id=100&status=maybe")
        .with_session(sessions::DEALER)
        .await
        .assert_status_bad_request();
    app.get("/v1/deals?group_id=100")
        .await
        .assert_status_unauthorized();
    app.get("/v1/deals?group_id=100")
        .with_session(sessions::VIEWER)
        .await
        .assert_status(StatusCode::FORBIDDEN);
}

// ---- one deal ----------------------------------------------------------------

#[test_db::test(seed)]
async fn get_one_deal(pool: MySqlPool) {
    assert_eq!(
        get(&pool, "/v1/deals/8600", sessions::DEALER).await,
        json!({
            "id": 8600,
            "title": "Dina kitchen",
            "amount": "1500.25",
            "list_id": 100,
            "group_id": 100,
            "position": 0,
            "outcome": "open",
            "lost_reason": null,
            "due_date": "2026-09-30",
            "user_id": 107,
            "customer_id": 8695,
            "customer_name": "Nina Lead",
            "customer_deal_count": 2,
            "files_count": 1,
            "created_at": "2026-04-01T10:00:00.000Z"
        })
    );
    // Any employee of the company may open it; others and deleted deals 404.
    get(&pool, "/v1/deals/8600", sessions::REP).await;
    let app = server(pool);
    app.get("/v1/deals/8600")
        .with_session(sessions::OTHER_COMPANY_REP)
        .await
        .assert_status_not_found();
    app.get("/v1/deals/8605")
        .with_session(sessions::DEALER)
        .await
        .assert_status_not_found();
}

#[test_db::test(seed)]
async fn stage_history_oldest_first(pool: MySqlPool) {
    assert_eq!(
        get(&pool, "/v1/deals/8602/stage-history", sessions::DEALER).await,
        json!([
            { "list_id": 100, "entered_at": "2026-04-03T10:00:00.000Z", "exited_at": "2026-04-03T12:00:00.000Z" },
            { "list_id": 101, "entered_at": "2026-04-03T12:00:00.000Z", "exited_at": null }
        ])
    );
    server(pool)
        .get("/v1/deals/8009/stage-history")
        .with_session(sessions::DEALER)
        .await
        .assert_status_not_found();
}

// ---- create ------------------------------------------------------------------

#[test_db::test(seed)]
async fn create_a_deal_for_a_customer_without_one(pool: MySqlPool) {
    // Hank (1007) has no deals and no sales rep; Rita (100) is a sales rep.
    let res = server(pool.clone())
        .post("/v1/deals")
        .with_session(sessions::REP)
        .json(&json!({ "customer_id": 1007, "list_id": 101, "title": "Hank job", "amount": 12.5 }))
        .await;
    res.assert_status(StatusCode::CREATED);
    let body: Value = res.json();
    assert_eq!(body["created"], true);
    let id = body["id"].as_i64().unwrap();
    assert_eq!(
        rows(&pool, &format!(
            "SELECT title, CAST(amount AS CHAR), status, CAST(list_id AS CHAR), CAST(user_id AS CHAR), created_by
             FROM deals WHERE id = {id}"
        ))
        .await,
        vec![vec!["Hank job", "12.50", "Quoted", "101", "100", "Rita Rep"]]
    );
    assert_eq!(
        scalar_i64(&pool, &format!("SELECT COUNT(*) FROM deal_stage_history WHERE deal_id = {id} AND list_id = 101 AND exited_at IS NULL")).await,
        1
    );
    // The rep becomes Hank's sales rep, with an audit row.
    assert_eq!(
        scalar_i64(&pool, "SELECT sales_rep FROM customers WHERE id = 1007").await,
        100
    );
    assert_eq!(
        rows(
            &pool,
            "SELECT reassigned_by, reassigned_to FROM customers_history WHERE customer_id = 1007"
        )
        .await,
        vec![vec!["Rita Rep", "Rita Rep"]]
    );
    // Hank is not a lead: no drips.
    assert_eq!(
        scalar_i64(
            &pool,
            &format!("SELECT COUNT(*) FROM scheduled_emails WHERE deal_id = {id}")
        )
        .await,
        0
    );
}

#[test_db::test(seed)]
async fn creating_for_a_lead_schedules_the_stage_drips(pool: MySqlPool) {
    // A new lead without deals, in Ada's company.
    sqlx::query("INSERT INTO customers (id, name, company_id, source, sales_rep) VALUES (8690, 'New Lead', 100, 'Leads', 107)")
        .execute(&pool)
        .await
        .unwrap();
    let res = server(pool.clone())
        .post("/v1/deals")
        .with_session(sessions::DEALER)
        .json(&json!({ "customer_id": 8690, "list_id": 101 }))
        .await;
    res.assert_status(StatusCode::CREATED);
    let id = res.json::<Value>()["id"].as_i64().unwrap();
    // Owned by the customer's rep.
    assert_eq!(
        scalar_i64(&pool, &format!("SELECT user_id FROM deals WHERE id = {id}")).await,
        107
    );
    assert_eq!(
        rows(&pool, &format!(
            "SELECT CAST(template_id AS CHAR), CAST(user_id AS CHAR), status, CAST(send_at > NOW() AS CHAR)
             FROM scheduled_emails WHERE deal_id = {id}"
        ))
        .await,
        vec![vec!["8670", "107", "pending", "1"]]
    );
    assert_eq!(
        scalar_i64(&pool, &format!("SELECT COUNT(*) FROM scheduled_sms WHERE deal_id = {id} AND template_id = 8671 AND status = 'pending'")).await,
        1
    );
}

#[test_db::test(seed)]
async fn creating_reuses_the_customers_open_deal(pool: MySqlPool) {
    // Erin (1004) has open deals 8002 (Rita's, the oldest) and 8601.
    let res = server(pool.clone())
        .post("/v1/deals")
        .with_session(sessions::DEALER)
        .json(&json!({ "customer_id": 1004, "list_id": 100, "title": "Reused" }))
        .await;
    res.assert_status(StatusCode::CREATED);
    assert_eq!(res.json::<Value>(), json!({ "id": 8002, "created": false }));
    assert_eq!(
        rows(
            &pool,
            "SELECT title, CAST(list_id AS CHAR), status FROM deals WHERE id = 8002"
        )
        .await,
        vec![vec!["Reused", "100", "New"]]
    );
    // Moved from stage 101: history follows.
    assert_eq!(
        scalar_i64(
            &pool,
            "SELECT list_id FROM deal_stage_history WHERE deal_id = 8002 AND exited_at IS NULL"
        )
        .await,
        100
    );
}

#[test_db::test(seed)]
async fn create_validates_customer_and_stage(pool: MySqlPool) {
    let app = server(pool);
    for body in [
        json!({ "customer_id": 2000, "list_id": 100 }),
        json!({ "customer_id": 1003, "list_id": 100 }),
        json!({ "customer_id": 1000, "list_id": 103 }),
        json!({ "customer_id": 1000, "list_id": 102 }),
        json!({ "customer_id": 1000 }),
    ] {
        app.post("/v1/deals")
            .with_session(sessions::DEALER)
            .json(&body)
            .await
            .assert_status_bad_request();
    }
}

// ---- update, reorder, outcome, delete ----------------------------------------------

#[test_db::test(seed)]
async fn patch_changes_only_the_fields_sent(pool: MySqlPool) {
    let app = server(pool.clone());
    app.patch("/v1/deals/8600")
        .with_session(sessions::DEALER)
        .json(&json!({ "amount": 99.5 }))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    app.patch("/v1/deals/8600")
        .with_session(sessions::DEALER)
        .json(&json!({ "due_date": null, "title": null }))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    assert_eq!(
        rows(&pool, "SELECT title, CAST(amount AS CHAR), due_date, CAST(list_id AS CHAR) FROM deals WHERE id = 8600").await,
        vec![vec!["NULL", "99.50", "NULL", "100"]]
    );
    app.patch("/v1/deals/8600")
        .with_session(sessions::DEALER)
        .json(&json!({ "due_date": "2026-12-01", "amount": null }))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    assert_eq!(
        rows(
            &pool,
            "SELECT CAST(due_date AS CHAR), CAST(amount AS CHAR) FROM deals WHERE id = 8600"
        )
        .await,
        vec![vec!["2026-12-01", "NULL"]]
    );
    // Bad values and other companies.
    for body in [
        json!({ "due_date": "12/01/2026" }),
        json!({ "list_id": 103 }),
        json!({ "customer_id": 2000 }),
    ] {
        app.patch("/v1/deals/8600")
            .with_session(sessions::DEALER)
            .json(&body)
            .await
            .assert_status_bad_request();
    }
    app.patch("/v1/deals/8009")
        .with_session(sessions::DEALER)
        .json(&json!({ "amount": 1 }))
        .await
        .assert_status_not_found();
}

#[test_db::test(seed)]
async fn moving_a_lost_deal_reopens_it(pool: MySqlPool) {
    // 8604 is lost in stage 101; customer 1011 is not a lead.
    server(pool.clone())
        .patch("/v1/deals/8604")
        .with_session(sessions::DEALER)
        .json(&json!({ "list_id": 100, "position": 4 }))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    assert_eq!(
        rows(&pool, "SELECT CAST(is_won AS CHAR), lost_reason, status, CAST(list_id AS CHAR), CAST(position AS CHAR) FROM deals WHERE id = 8604").await,
        vec![vec!["NULL", "NULL", "New", "100", "4"]]
    );
    assert_eq!(
        scalar_i64(
            &pool,
            "SELECT list_id FROM deal_stage_history WHERE deal_id = 8604 AND exited_at IS NULL"
        )
        .await,
        100
    );
}

#[test_db::test(seed)]
async fn moving_a_lead_deal_reschedules_drips(pool: MySqlPool) {
    // 8600 (Nina, a lead) moves to 101, which has drip templates; a pending
    // email of the old stage is cancelled.
    sqlx::query(
        "INSERT INTO scheduled_emails (template_id, deal_id, list_id, customer_id, user_id, company_id, send_at, status)
         VALUES (8670, 8600, 100, 8695, 107, 100, NOW() + INTERVAL 1 DAY, 'pending')",
    )
    .execute(&pool)
    .await
    .unwrap();
    server(pool.clone())
        .patch("/v1/deals/8600")
        .with_session(sessions::DEALER)
        .json(&json!({ "list_id": 101 }))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    assert_eq!(
        rows(&pool, "SELECT status, error_message, CAST(list_id AS CHAR) FROM scheduled_emails WHERE deal_id = 8600 ORDER BY id").await,
        vec![
            vec!["cancelled", "Deal moved lists", "100"],
            vec!["pending", "NULL", "101"],
        ]
    );
}

#[test_db::test(seed)]
async fn reorder_saves_positions_and_moves(pool: MySqlPool) {
    server(pool.clone())
        .post("/v1/deals/reorder")
        .with_session(sessions::DEALER)
        .json(&json!({ "updates": [
            { "id": 8601, "list_id": 100, "position": 0 },
            { "id": 8600, "list_id": 101, "position": 3 }
        ]}))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    assert_eq!(
        rows(&pool, "SELECT CAST(id AS CHAR), CAST(list_id AS CHAR), CAST(position AS CHAR), status FROM deals WHERE id IN (8600, 8601) ORDER BY id").await,
        vec![vec!["8600", "101", "3", "Quoted"], vec!["8601", "100", "0", "New"]]
    );
    assert_eq!(
        scalar_i64(
            &pool,
            "SELECT COUNT(*) FROM deal_stage_history WHERE deal_id = 8601"
        )
        .await,
        0,
        "same stage: no history"
    );
    let app = server(pool);
    app.post("/v1/deals/reorder")
        .with_session(sessions::DEALER)
        .json(&json!({ "updates": [{ "id": 8009, "list_id": 100, "position": 0 }] }))
        .await
        .assert_status_not_found();
    app.post("/v1/deals/reorder")
        .with_session(sessions::DEALER)
        .json(&json!({ "updates": [] }))
        .await
        .assert_status_bad_request();
}

#[test_db::test(seed)]
async fn outcome_won_lost_and_reopen(pool: MySqlPool) {
    let app = server(pool.clone());
    let set = |id: i64, body: Value| {
        app.put(&format!("/v1/deals/{id}/outcome"))
            .with_session(sessions::DEALER)
            .json(&body)
    };
    set(8600, json!({ "outcome": "won" }))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    assert_eq!(
        rows(
            &pool,
            "SELECT CAST(is_won AS CHAR), due_date FROM deals WHERE id = 8600"
        )
        .await,
        vec![vec!["1", "NULL"]]
    );
    set(
        8601,
        json!({ "outcome": "lost", "lost_reason": " Chose competitor " }),
    )
    .await
    .assert_status(StatusCode::NO_CONTENT);
    assert_eq!(
        rows(
            &pool,
            "SELECT CAST(is_won AS CHAR), lost_reason FROM deals WHERE id = 8601"
        )
        .await,
        vec![vec!["0", "Chose competitor"]]
    );
    // Reopen: back in its stage with a new history row.
    set(8603, json!({ "outcome": "open" }))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    assert_eq!(
        rows(
            &pool,
            "SELECT CAST(is_won AS CHAR), CAST(list_id AS CHAR) FROM deals WHERE id = 8603"
        )
        .await,
        vec![vec!["NULL", "101"]]
    );
    assert_eq!(
        scalar_i64(
            &pool,
            "SELECT COUNT(*) FROM deal_stage_history WHERE deal_id = 8603 AND exited_at IS NULL"
        )
        .await,
        1
    );
    set(8009, json!({ "outcome": "won" }))
        .await
        .assert_status_not_found();
    set(8600, json!({ "outcome": "maybe" }))
        .await
        .assert_status_bad_request();
}

#[test_db::test(seed)]
async fn reopening_a_deal_in_a_deleted_stage_uses_its_last_live_stage(pool: MySqlPool) {
    // 8007 (Rita's) sits in deleted stage 102 with no history.
    sqlx::query("UPDATE deals SET is_won = 0 WHERE id = 8007")
        .execute(&pool)
        .await
        .unwrap();
    server(pool.clone())
        .put("/v1/deals/8007/outcome")
        .with_session(sessions::REP)
        .json(&json!({ "outcome": "open" }))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    // First stage of its own group.
    assert_eq!(
        scalar_i64(&pool, "SELECT list_id FROM deals WHERE id = 8007").await,
        100
    );
}

#[test_db::test(seed)]
async fn delete_soft_deletes_and_audits(pool: MySqlPool) {
    let app = server(pool.clone());
    app.delete("/v1/deals/8601")
        .with_session(sessions::DEALER)
        .await
        .assert_status(StatusCode::NO_CONTENT);
    assert_eq!(
        scalar_i64(
            &pool,
            "SELECT deleted_at IS NOT NULL FROM deals WHERE id = 8601"
        )
        .await,
        1
    );
    assert_eq!(
        rows(
            &pool,
            "SELECT reassigned_by, reassigned_to FROM customers_history WHERE customer_id = 1004"
        )
        .await,
        vec![vec!["Dina Dealer", "deal_deleted:8601"]]
    );
    app.delete("/v1/deals/8601")
        .with_session(sessions::DEALER)
        .await
        .assert_status_not_found();
    app.delete("/v1/deals/8009")
        .with_session(sessions::DEALER)
        .await
        .assert_status_not_found();
}

// ---- activities --------------------------------------------------------------------

#[test_db::test(seed)]
async fn list_activities_newest_first(pool: MySqlPool) {
    let list = get(&pool, "/v1/deals/8601/activities", sessions::DEALER).await;
    assert_eq!(ids(&list), vec![8623, 8622]);
    assert_eq!(
        list[0],
        json!({
            "id": 8623,
            "deal_id": 8601,
            "name": "Old task",
            "deadline": "2026-01-01T10:00:00",
            "priority": "medium",
            "is_completed": true,
            "completed_at": "2026-01-02T10:00:00.000Z",
            "created_at": "2026-04-02T12:00:00.000Z",
            "created_by": "Dina Dealer"
        })
    );
    server(pool)
        .get("/v1/deals/8009/activities")
        .with_session(sessions::DEALER)
        .await
        .assert_status_not_found();
}

#[test_db::test(seed)]
async fn create_activity_notifies_and_schedules_a_reminder(pool: MySqlPool) {
    // Rita adds an activity to Dina's deal 8600 (Nina's rep is Rita herself).
    let res = server(pool.clone())
        .post("/v1/deals/8600/activities")
        .with_session(sessions::REP)
        .json(&json!({ "name": "  Site visit ", "deadline": "2026-06-01T14:30:00", "priority": "high" }))
        .await;
    res.assert_status(StatusCode::CREATED);
    let id = res.json::<Value>()["id"].as_i64().unwrap();
    assert_eq!(
        rows(&pool, &format!("SELECT name, CAST(deadline AS CHAR), CAST(priority AS CHAR), created_by FROM deal_activities WHERE id = {id}")).await,
        vec![vec!["Site visit", "2026-06-01 14:30:00", "high", "Rita Rep"]]
    );
    // Dina (owner) is notified; Rita (actor and rep) is not. The reminder for
    // the owner fires at 14:30 Indianapolis (EDT) = 18:30 UTC.
    assert_eq!(
        rows(&pool, "SELECT CAST(user_id AS CHAR), notification_type, message, actor_name FROM notifications WHERE deal_id = 8600 ORDER BY id").await,
        vec![
            vec!["107", "activity_added", "Site visit", "Rita Rep"],
            vec!["107", "activity_deadline_reminder", "Site visit", "NULL"],
        ]
    );
    assert_eq!(
        rows(&pool, "SELECT CAST(due_at AS CHAR) FROM notifications WHERE notification_type = 'activity_deadline_reminder'").await,
        vec![vec!["2026-06-01 18:30:00"]]
    );
}

#[test_db::test(seed)]
async fn activity_validation(pool: MySqlPool) {
    let app = server(pool);
    for body in [
        json!({ "name": "  " }),
        json!({ "name": "x".repeat(256) }),
        json!({ "name": "ok", "deadline": "tomorrow" }),
        json!({ "name": "ok", "deadline": "2026-13-01" }),
        json!({ "name": "ok", "priority": "urgent" }),
    ] {
        app.post("/v1/deals/8600/activities")
            .with_session(sessions::DEALER)
            .json(&body)
            .await
            .assert_status_bad_request();
    }
    app.post("/v1/deals/8009/activities")
        .with_session(sessions::DEALER)
        .json(&json!({ "name": "ok" }))
        .await
        .assert_status_not_found();
}

#[test_db::test(seed)]
async fn date_only_deadline_gets_no_reminder(pool: MySqlPool) {
    server(pool.clone())
        .post("/v1/deals/8600/activities")
        .with_session(sessions::DEALER)
        .json(&json!({ "name": "All day", "deadline": "2026-06-01" }))
        .await
        .assert_status(StatusCode::CREATED);
    assert_eq!(
        scalar_i64(&pool, "SELECT COUNT(*) FROM notifications WHERE notification_type = 'activity_deadline_reminder'").await,
        0
    );
    assert_eq!(
        rows(
            &pool,
            "SELECT CAST(deadline AS CHAR) FROM deal_activities WHERE name = 'All day'"
        )
        .await,
        vec![vec!["2026-06-01 00:00:00"]]
    );
}

#[test_db::test(seed)]
async fn edit_complete_and_delete_an_activity(pool: MySqlPool) {
    let app = server(pool.clone());
    app.patch("/v1/deals/8600/activities/8621")
        .with_session(sessions::DEALER)
        .json(&json!({ "name": "Send final quote", "deadline": "2026-07-01 09:00:00" }))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    assert_eq!(
        rows(&pool, "SELECT name, CAST(deadline AS CHAR), CAST(priority AS CHAR) FROM deal_activities WHERE id = 8621").await,
        vec![vec!["Send final quote", "2026-07-01 09:00:00", "low"]]
    );
    assert_eq!(
        rows(&pool, "SELECT message, CAST(due_at AS CHAR) FROM notifications WHERE notification_type = 'activity_deadline_reminder'").await,
        vec![vec!["Send final quote", "2026-07-01 13:00:00"]]
    );
    // Done: completed_at stamped, reminder gone.
    app.patch("/v1/deals/8600/activities/8621")
        .with_session(sessions::DEALER)
        .json(&json!({ "is_completed": true }))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    assert_eq!(
        scalar_i64(
            &pool,
            "SELECT is_completed AND completed_at IS NOT NULL FROM deal_activities WHERE id = 8621"
        )
        .await,
        1
    );
    assert_eq!(
        scalar_i64(&pool, "SELECT COUNT(*) FROM notifications WHERE notification_type = 'activity_deadline_reminder'").await,
        0
    );
    app.patch("/v1/deals/8600/activities/8621")
        .with_session(sessions::DEALER)
        .json(&json!({ "is_completed": false }))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    assert_eq!(
        scalar_i64(
            &pool,
            "SELECT is_completed + (completed_at IS NOT NULL) FROM deal_activities WHERE id = 8621"
        )
        .await,
        0
    );
    app.delete("/v1/deals/8600/activities/8621")
        .with_session(sessions::DEALER)
        .await
        .assert_status(StatusCode::NO_CONTENT);
    assert_eq!(
        scalar_i64(
            &pool,
            "SELECT deleted_at IS NOT NULL FROM deal_activities WHERE id = 8621"
        )
        .await,
        1
    );
    // Activity of another deal, deleted ones.
    app.delete("/v1/deals/8601/activities/8620")
        .with_session(sessions::DEALER)
        .await
        .assert_status_not_found();
    app.patch("/v1/deals/8601/activities/8624")
        .with_session(sessions::DEALER)
        .json(&json!({ "is_completed": true }))
        .await
        .assert_status_not_found();
}

#[test_db::test(seed)]
async fn activity_suggestions_most_used_first(pool: MySqlPool) {
    // Dina wrote "Call back" twice; deleted ones do not count.
    let list = get(&pool, "/v1/deal-activities/suggestions", sessions::DEALER).await;
    assert_eq!(list[0], json!({ "name": "Call back", "uses": 2 }));
    assert!(
        list.as_array()
            .unwrap()
            .iter()
            .all(|s| s["name"] != "Deleted task")
    );
    assert_eq!(list.as_array().unwrap().len(), 4);
    assert_eq!(
        get(&pool, "/v1/deal-activities/suggestions", sessions::REP).await,
        json!([])
    );
}

#[test_db::test(seed)]
async fn hidden_activity_suggestions_never_come_back(pool: MySqlPool) {
    let app = server(pool.clone());
    let names = |list: Value| -> Vec<String> {
        list.as_array()
            .unwrap()
            .iter()
            .map(|s| s["name"].as_str().unwrap().to_string())
            .collect()
    };
    // Dina hides "Call back" (stray spaces trimmed; hiding it twice is fine).
    for _ in 0..2 {
        app.post("/v1/deal-activities/suggestions/hidden")
            .with_session(sessions::DEALER)
            .json(&json!({ "name": " Call back " }))
            .await
            .assert_status(StatusCode::NO_CONTENT);
    }
    let list = names(get(&pool, "/v1/deal-activities/suggestions", sessions::DEALER).await);
    assert_eq!(list.len(), 3);
    assert!(!list.contains(&"Call back".to_string()));
    // Using the name again does not bring it back.
    sqlx::query(
        "INSERT INTO deal_activities (deal_id, company_id, name, created_by)
         VALUES (8600, 100, 'Call back', 'Dina Dealer')",
    )
    .execute(&pool)
    .await
    .unwrap();
    let list = names(get(&pool, "/v1/deal-activities/suggestions", sessions::DEALER).await);
    assert!(!list.contains(&"Call back".to_string()));
    // No name: nothing to hide.
    app.post("/v1/deal-activities/suggestions/hidden")
        .with_session(sessions::DEALER)
        .json(&json!({ "name": "  " }))
        .await
        .assert_status_bad_request();
}

// ---- notes ---------------------------------------------------------------------------

#[test_db::test(seed)]
async fn notes_with_comments(pool: MySqlPool) {
    assert_eq!(
        get(&pool, "/v1/deals/8600/notes", sessions::DEALER).await,
        json!([
            { "id": 8631, "deal_id": 8600, "content": "Second note", "is_pinned": false,
              "created_at": "2026-04-01T14:00:00.000Z", "created_by": "Dina Dealer", "comments": [] },
            { "id": 8630, "deal_id": 8600, "content": "Prefers white quartz", "is_pinned": true,
              "created_at": "2026-04-01T13:00:00.000Z", "created_by": "Dina Dealer", "comments": [
                { "id": 8640, "note_id": 8630, "content": "Agreed", "created_at": "2026-04-01T15:00:00.000Z", "created_by": "Ada Admin" },
                { "id": 8642, "note_id": 8630, "content": "Anonymous", "created_at": "2026-04-01T17:00:00.000Z", "created_by": null }
              ] }
        ])
    );
    assert_eq!(
        get(&pool, "/v1/deals/8602/notes", sessions::DEALER).await,
        json!([])
    );
}

#[test_db::test(seed)]
async fn write_notes_and_comments(pool: MySqlPool) {
    let app = server(pool.clone());
    let res = app
        .post("/v1/deals/8601/notes")
        .with_session(sessions::DEALER)
        .json(&json!({ "content": " New note " }))
        .await;
    res.assert_status(StatusCode::CREATED);
    let note = res.json::<Value>()["id"].as_i64().unwrap();
    // Erin's rep is Mark (101): notified.
    assert_eq!(
        rows(&pool, "SELECT CAST(user_id AS CHAR), notification_type, message FROM notifications WHERE deal_id = 8601").await,
        vec![vec!["101", "note_added", "New note"]]
    );
    app.patch(&format!("/v1/deals/8601/notes/{note}"))
        .with_session(sessions::DEALER)
        .json(&json!({ "is_pinned": true, "content": "Edited" }))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    assert_eq!(
        rows(
            &pool,
            &format!("SELECT content, CAST(is_pinned AS CHAR) FROM deal_notes WHERE id = {note}")
        )
        .await,
        vec![vec!["Edited", "1"]]
    );
    // A non-admin's comment is anonymous and notifies nobody.
    let res = app
        .post(&format!("/v1/deals/8601/notes/{note}/comments"))
        .with_session(sessions::DEALER)
        .json(&json!({ "content": "Me too" }))
        .await;
    res.assert_status(StatusCode::CREATED);
    let comment = res.json::<Value>()["id"].as_i64().unwrap();
    assert_eq!(
        rows(
            &pool,
            &format!("SELECT created_by FROM deal_note_comments WHERE id = {comment}")
        )
        .await,
        vec![vec!["NULL"]]
    );
    assert_eq!(
        scalar_i64(
            &pool,
            "SELECT COUNT(*) FROM notifications WHERE notification_type = 'comment_added'"
        )
        .await,
        0
    );
    // An admin's comment carries the name and notifies.
    app.post(&format!("/v1/deals/8601/notes/{note}/comments"))
        .with_session(sessions::ADMIN)
        .json(&json!({ "content": "Approved" }))
        .await
        .assert_status(StatusCode::CREATED);
    assert_eq!(
        scalar_i64(
            &pool,
            "SELECT COUNT(*) FROM notifications WHERE notification_type = 'comment_added'"
        )
        .await,
        2
    );
    app.delete(&format!("/v1/deals/8601/notes/{note}/comments/{comment}"))
        .with_session(sessions::DEALER)
        .await
        .assert_status(StatusCode::NO_CONTENT);
    app.delete(&format!("/v1/deals/8601/notes/{note}/comments/{comment}"))
        .with_session(sessions::DEALER)
        .await
        .assert_status_not_found();
    app.delete(&format!("/v1/deals/8601/notes/{note}"))
        .with_session(sessions::ADMIN)
        .await
        .assert_status(StatusCode::NO_CONTENT);
    assert_eq!(
        scalar_i64(
            &pool,
            "SELECT COUNT(*) FROM notifications WHERE notification_type = 'note_deleted'"
        )
        .await,
        2,
        "admin deletion notifies owner and rep"
    );
}

#[test_db::test(seed)]
async fn note_validation_and_scoping(pool: MySqlPool) {
    let app = server(pool);
    app.post("/v1/deals/8600/notes")
        .with_session(sessions::DEALER)
        .json(&json!({ "content": "" }))
        .await
        .assert_status_bad_request();
    app.post("/v1/deals/8600/notes")
        .with_session(sessions::DEALER)
        .json(&json!({ "content": "x".repeat(5001) }))
        .await
        .assert_status_bad_request();
    app.post("/v1/deals/8600/notes/8630/comments")
        .with_session(sessions::DEALER)
        .json(&json!({ "content": "x".repeat(2001) }))
        .await
        .assert_status_bad_request();
    app.post("/v1/deals/8009/notes")
        .with_session(sessions::DEALER)
        .json(&json!({ "content": "x" }))
        .await
        .assert_status_not_found();
    // Note of another deal, deleted note.
    app.patch("/v1/deals/8601/notes/8630")
        .with_session(sessions::DEALER)
        .json(&json!({ "is_pinned": false }))
        .await
        .assert_status_not_found();
    app.delete("/v1/deals/8602/notes/8632")
        .with_session(sessions::DEALER)
        .await
        .assert_status_not_found();
}

// ---- by customer, recipients --------------------------------------------------

#[test_db::test(seed)]
async fn deals_of_a_customer_callers_first(pool: MySqlPool) {
    // Nina: Dina's 8600/8607 (8605 deleted). Alice: Rita's 8000/8004/8007.
    let deals = get(
        &pool,
        "/v1/deals/by-customer?customer_id=8695",
        sessions::DEALER,
    )
    .await;
    assert_eq!(ids(&deals), vec![8607, 8600]);
    let alice = get(
        &pool,
        "/v1/deals/by-customer?customer_id=1000",
        sessions::DEALER,
    )
    .await;
    assert_eq!(ids(&alice), vec![8007, 8004, 8000]);
    assert_eq!(
        deals[1],
        json!({
            "id": 8600,
            "title": "Dina kitchen",
            "status": "New",
            "amount": "1500.25",
            "list_name": "New",
            "notes": [
                { "id": 8630, "content": "Prefers white quartz", "created_at": "2026-04-01T13:00:00.000Z" },
                { "id": 8631, "content": "Second note", "created_at": "2026-04-01T14:00:00.000Z" }
            ],
            "activities": [
                { "id": 8620, "name": "Call back", "deadline": "2026-01-05T09:30:00", "priority": "high", "is_completed": false },
                { "id": 8621, "name": "Send quote", "deadline": "2099-01-01T00:00:00", "priority": "low", "is_completed": false }
            ]
        })
    );
    // By email (any address, any case) and by exact name.
    let by_email = get(
        &pool,
        "/v1/deals/by-customer?customer_email=NINA.WORK@example.com",
        sessions::DEALER,
    )
    .await;
    assert_eq!(ids(&by_email), ids(&deals));
    let by_name = get(
        &pool,
        "/v1/deals/by-customer?customer_name=erin%20builder",
        sessions::REP,
    )
    .await;
    assert_eq!(ids(&by_name), vec![8010, 8002, 8601]);
    // Other company's customer: nothing. No criteria: 400.
    assert_eq!(
        get(
            &pool,
            "/v1/deals/by-customer?customer_id=2000",
            sessions::DEALER
        )
        .await,
        json!([])
    );
    server(pool)
        .get("/v1/deals/by-customer")
        .with_session(sessions::DEALER)
        .await
        .assert_status_bad_request();
}

#[test_db::test(seed)]
async fn stage_recipients_are_the_callers_customers(pool: MySqlPool) {
    // Rita is the rep of Alice (deal 8000 won in 101) and Jack (Dina's lost 8604 in 101).
    assert_eq!(
        get(&pool, "/v1/deal-lists/101/recipients", sessions::REP).await,
        json!([])
    );
    assert_eq!(
        get(
            &pool,
            "/v1/deal-lists/101/recipients?include_won=true&include_lost=true",
            sessions::REP
        )
        .await,
        json!([
            { "email": "alice.old@example.com", "name": "Alice Lead" },
            { "email": "alice.work@example.com", "name": "Alice Lead" },
            { "email": "alice@example.com", "name": "Alice Lead" },
            { "email": "jack@example.com", "name": "Jack Mismatched Email" }
        ])
    );
    assert_eq!(
        get(
            &pool,
            "/v1/deal-lists/101/recipients?include_lost=true",
            sessions::REP
        )
        .await,
        json!([{ "email": "jack@example.com", "name": "Jack Mismatched Email" }])
    );
}

// ---- group move, files -----------------------------------------------------

#[test_db::test(seed)]
async fn patch_group_moves_to_its_first_stage(pool: MySqlPool) {
    let app = server(pool.clone());
    // Same group: nothing moves (a remounted select resends it).
    app.patch("/v1/deals/8603")
        .with_session(sessions::DEALER)
        .json(&json!({ "group_id": 100 }))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    assert_eq!(
        rows(
            &pool,
            "SELECT CAST(list_id AS CHAR), CAST(is_won AS CHAR) FROM deals WHERE id = 8603"
        )
        .await,
        vec![vec!["101", "1"]]
    );
    // Won deal to Remodel: first stage, reopened.
    app.patch("/v1/deals/8603")
        .with_session(sessions::DEALER)
        .json(&json!({ "group_id": 104 }))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    assert_eq!(
        rows(
            &pool,
            "SELECT CAST(list_id AS CHAR), CAST(is_won AS CHAR), status FROM deals WHERE id = 8603"
        )
        .await,
        vec![vec!["104", "NULL", "Measure"]]
    );
    for group in [105, 101] {
        app.patch("/v1/deals/8603")
            .with_session(sessions::DEALER)
            .json(&json!({ "group_id": group }))
            .await
            .assert_status_bad_request();
    }
}

#[test_db::test(seed)]
async fn deal_files_attach_list_rename_delete(pool: MySqlPool) {
    let (app, storage) = common::server_with_storage(pool.clone());
    // Upload two files to the deals folder like the browser would.
    let mut urls = Vec::new();
    for (name, content_type, bytes) in [
        ("kitchen.png", "image/png", common::png(20, 10)),
        ("Quote.pdf", "application/pdf", b"%PDF-1.4".to_vec()),
    ] {
        let res = app
            .post("/v1/uploads")
            .with_session(sessions::DEALER)
            .json(&json!({ "folder": "deals", "file_name": name, "content_type": content_type }))
            .await;
        res.assert_status(StatusCode::CREATED);
        let url = res.json::<Value>()["url"].as_str().unwrap().to_string();
        let key = storage.key_for_url(&url).unwrap();
        storage.put(&key, bytes, content_type).await.unwrap();
        urls.push((url, name));
    }
    let res = app
        .post("/v1/deals/8601/files")
        .with_session(sessions::DEALER)
        .json(&json!({ "files": urls.iter().map(|(url, name)| json!({ "url": url, "name": name })).collect::<Vec<_>>() }))
        .await;
    res.assert_status(StatusCode::CREATED);
    assert_eq!(res.json::<Value>(), json!({ "images": 1, "documents": 1 }));

    let files: Value = app
        .get("/v1/deals/8601/files")
        .with_session(sessions::DEALER)
        .await
        .json();
    assert_eq!(files["images"].as_array().unwrap().len(), 1);
    assert_eq!(files["images"][0]["url"], urls[0].0.as_str());
    assert_eq!(files["documents"][0]["name"], "Quote.pdf");
    assert_eq!(files["documents"][0]["source"], "documents");
    let doc_id = files["documents"][0]["id"].as_i64().unwrap();

    app.patch(&format!("/v1/deals/8601/documents/{doc_id}"))
        .with_session(sessions::DEALER)
        .json(&json!({ "name": "Final quote" }))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    // Not this deal's document.
    app.patch(&format!("/v1/deals/8600/documents/{doc_id}"))
        .with_session(sessions::DEALER)
        .json(&json!({ "name": "x" }))
        .await
        .assert_status_not_found();
    app.delete(&format!("/v1/deals/8601/files/documents/{doc_id}"))
        .with_session(sessions::DEALER)
        .await
        .assert_status(StatusCode::NO_CONTENT);
    assert!(!storage.memory_keys().iter().any(|k| k.ends_with(".pdf")));
    let files: Value = app
        .get("/v1/deals/8601/files")
        .with_session(sessions::DEALER)
        .await
        .json();
    assert_eq!(files["documents"], json!([]));

    // A URL that was never uploaded, a foreign deal, a bad source.
    app.post("/v1/deals/8601/files")
        .with_session(sessions::DEALER)
        .json(&json!({ "files": [{ "url": "https://evil.test/x.png" }] }))
        .await
        .assert_status_bad_request();
    app.get("/v1/deals/8009/files")
        .with_session(sessions::DEALER)
        .await
        .assert_status_not_found();
    app.delete("/v1/deals/8601/files/other/1")
        .with_session(sessions::DEALER)
        .await
        .assert_status_bad_request();
}

#[test_db::test(seed)]
async fn seeded_photo_and_documents_kept_in_images(pool: MySqlPool) {
    sqlx::query("INSERT INTO deals_images (id, deal_id, image_url, created_at) VALUES (8651, 8600, 'https://example.test/old-quote.PDF?x=1', '2026-04-02 10:00:00')")
        .execute(&pool)
        .await
        .unwrap();
    let files = get(&pool, "/v1/deals/8600/files", sessions::DEALER).await;
    assert_eq!(
        files,
        json!({
            "images": [{ "id": 8650, "url": "https://example.test/deal-8600.jpg", "created_at": "2026-04-01T10:00:00.000Z" }],
            "documents": [{ "id": 8651, "url": "https://example.test/old-quote.PDF?x=1", "created_at": "2026-04-02T10:00:00.000Z", "source": "images", "name": null }]
        })
    );
}

// ---- negative amounts and reused open deals (QA audit 2026-10-14) -------------

#[test_db::test(seed)]
async fn creating_for_a_customer_with_an_open_deal_keeps_its_amount_and_title(pool: MySqlPool) {
    // Erin (1004) has open deal 8002. A create that leaves amount and title
    // empty must not wipe them (the deal is only reused and moved).
    let before = rows(
        &pool,
        "SELECT CAST(amount AS CHAR), title FROM deals WHERE id = 8002",
    )
    .await;
    let res = server(pool.clone())
        .post("/v1/deals")
        .with_session(sessions::DEALER)
        .json(&json!({ "customer_id": 1004, "list_id": 100 }))
        .await;
    res.assert_status(StatusCode::CREATED);
    assert_eq!(res.json::<Value>(), json!({ "id": 8002, "created": false }));
    assert_eq!(
        rows(
            &pool,
            "SELECT CAST(amount AS CHAR), title FROM deals WHERE id = 8002"
        )
        .await,
        before
    );
}

#[test_db::test(seed)]
async fn negative_amounts_are_rejected_on_create_and_patch(pool: MySqlPool) {
    let app = server(pool.clone());
    let before = rows(
        &pool,
        "SELECT CAST(amount AS CHAR) FROM deals WHERE id = 8600",
    )
    .await;
    app.post("/v1/deals")
        .with_session(sessions::DEALER)
        .json(&json!({ "customer_id": 1000, "list_id": 100, "amount": -5 }))
        .await
        .assert_status_bad_request();
    app.patch("/v1/deals/8600")
        .with_session(sessions::DEALER)
        .json(&json!({ "amount": -5 }))
        .await
        .assert_status_bad_request();
    assert_eq!(
        rows(
            &pool,
            "SELECT CAST(amount AS CHAR) FROM deals WHERE id = 8600"
        )
        .await,
        before
    );
    // Zero is still a valid amount.
    app.patch("/v1/deals/8600")
        .with_session(sessions::DEALER)
        .json(&json!({ "amount": 0 }))
        .await
        .assert_status(StatusCode::NO_CONTENT);
}
