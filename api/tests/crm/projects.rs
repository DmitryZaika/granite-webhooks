use crate::common;

use axum::http::StatusCode;
use common::{WithSession, server, sessions};
use serde_json::{Value, json};
use sqlx::MySqlPool;

// Seed: in company 100 Rita (100) is a sales rep and owns the open deal 8002
// "Erin countertops" of Erin Builder (1004); Mark (101) is the Sales Manager;
// Ada (102) is the admin. Projects starts off.

async fn run(pool: &MySqlPool, sql: &str) {
    sqlx::raw_sql(sqlx::AssertSqlSafe(sql.to_string()))
        .execute(pool)
        .await
        .unwrap();
}

async fn turn_on(pool: &MySqlPool) {
    run(
        pool,
        "UPDATE company SET projects_enabled = 1 WHERE id = 100",
    )
    .await;
}

async fn win_8002(pool: &MySqlPool) {
    server(pool.clone())
        .put("/v1/deals/8002/outcome")
        .with_session(sessions::ADMIN)
        .json(&json!({ "outcome": "won" }))
        .await
        .assert_status(StatusCode::NO_CONTENT);
}

async fn projects_of_8002(pool: &MySqlPool) -> Vec<i32> {
    sqlx::query_scalar("SELECT id FROM projects WHERE source_deal_id = 8002")
        .fetch_all(pool)
        .await
        .unwrap()
}

async fn board(pool: &MySqlPool, session: &str) -> Value {
    let res = server(pool.clone())
        .get("/v1/projects")
        .with_session(session)
        .await;
    res.assert_status_ok();
    res.json()
}

fn ids(board: &Value) -> Vec<i64> {
    board["projects"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["id"].as_i64().unwrap())
        .collect()
}

async fn act(pool: &MySqlPool, id: i32, session: &str, body: Value) -> axum_test::TestResponse {
    server(pool.clone())
        .post(&format!("/v1/projects/{id}/actions"))
        .with_session(session)
        .json(&body)
        .await
}

async fn version(pool: &MySqlPool, id: i32) -> i32 {
    sqlx::query_scalar("SELECT version FROM projects WHERE id = ?")
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap()
}

#[test_db::test(seed)]
async fn projects_stay_off_until_a_superuser_turns_them_on(pool: MySqlPool) {
    server(pool.clone())
        .get("/v1/projects")
        .with_session(sessions::ADMIN)
        .await
        .assert_status(StatusCode::NOT_FOUND);
    win_8002(&pool).await;
    assert!(projects_of_8002(&pool).await.is_empty());
}

#[test_db::test(seed)]
async fn a_sold_deal_goes_to_projects(pool: MySqlPool) {
    turn_on(&pool).await;
    win_8002(&pool).await;
    let created = projects_of_8002(&pool).await;
    assert_eq!(created.len(), 1);
    let id = created[0];

    let admin = board(&pool, sessions::ADMIN).await;
    assert_eq!(ids(&admin), [i64::from(id)]);
    let project = &admin["projects"][0];
    assert_eq!(project["stage_code"], "new");
    assert_eq!(project["title"], "Erin countertops");
    assert_eq!(project["customer_name"], "Erin Builder");
    assert_eq!(project["sales_rep_id"], 100);
    assert_eq!(
        admin["stages"],
        json!(["new", "templated", "fabrication", "installed", "completed"])
    );

    // Winning it again starts nothing more; the deal page finds it.
    win_8002(&pool).await;
    assert_eq!(projects_of_8002(&pool).await, [id]);
    let deal = server(pool.clone())
        .get("/v1/deals/8002/project")
        .with_session(sessions::REP)
        .await;
    deal.assert_status_ok();
    assert_eq!(deal.json::<Value>()["project_id"], id);
}

#[test_db::test(seed)]
async fn each_position_sees_its_part_of_the_board(pool: MySqlPool) {
    turn_on(&pool).await;
    win_8002(&pool).await;
    let id = projects_of_8002(&pool).await[0];
    let only = [i64::from(id)];

    // Rita sold it; Mark is the Sales Manager.
    assert_eq!(ids(&board(&pool, sessions::REP).await), only);
    assert_eq!(ids(&board(&pool, sessions::MANAGER).await), only);

    // Another rep's project is not Rita's.
    run(
        &pool,
        &format!("UPDATE projects SET sales_rep_id = 102 WHERE id = {id}"),
    )
    .await;
    assert!(ids(&board(&pool, sessions::REP).await).is_empty());
    server(pool.clone())
        .get(&format!("/v1/projects/{id}"))
        .with_session(sessions::REP)
        .await
        .assert_status(StatusCode::NOT_FOUND);

    // The Office Manager schedules: Awaiting Template to In Fabrication.
    run(
        &pool,
        "UPDATE users_positions SET position_id = 10 WHERE user_id = 101 AND company_id = 100",
    )
    .await;
    let office = board(&pool, sessions::MANAGER).await;
    assert_eq!(office["stages"], json!(["new", "templated", "fabrication"]));
    assert_eq!(ids(&office), only);

    // The Shop Manager: Templated onward.
    run(
        &pool,
        "UPDATE users_positions SET position_id = 3 WHERE user_id = 101 AND company_id = 100",
    )
    .await;
    let shop = board(&pool, sessions::MANAGER).await;
    assert_eq!(
        shop["stages"],
        json!(["templated", "fabrication", "installed", "completed"])
    );
    assert!(ids(&shop).is_empty());
    let moved = act(
        &pool,
        id,
        sessions::ADMIN,
        json!({ "intent": "stage", "version": version(&pool, id).await, "stage": "templated" }),
    )
    .await;
    moved.assert_status_ok();
    assert_eq!(moved.json::<Value>()["stage_code"], "templated");
    assert_eq!(ids(&board(&pool, sessions::MANAGER).await), only);

    // No position, no board.
    run(
        &pool,
        "DELETE FROM users_positions WHERE user_id = 101 AND company_id = 100",
    )
    .await;
    server(pool.clone())
        .get("/v1/projects")
        .with_session(sessions::MANAGER)
        .await
        .assert_status(StatusCode::FORBIDDEN);
}

#[test_db::test(seed)]
async fn a_task_tells_its_person_now_and_when_it_is_due(pool: MySqlPool) {
    turn_on(&pool).await;
    win_8002(&pool).await;
    let id = projects_of_8002(&pool).await[0];
    let server = server(pool.clone());

    // Only people of the company.
    server
        .post(&format!("/v1/projects/{id}/tasks"))
        .with_session(sessions::ADMIN)
        .json(&json!({ "name": "Expedite", "assignedUserId": 106 }))
        .await
        .assert_status(StatusCode::BAD_REQUEST);

    // Ada asks Mark to expedite the template; it was due long ago.
    let created = server
        .post(&format!("/v1/projects/{id}/tasks"))
        .with_session(sessions::ADMIN)
        .json(&json!({
            "name": "Expedite the template",
            "assignedUserId": 101,
            "deadline": "2020-01-01T10:00",
            "priority": "high",
        }))
        .await;
    created.assert_status(StatusCode::CREATED);
    let task = created.json::<Value>()["id"].as_i64().unwrap();

    let bell = |session: &'static str| {
        let request = server.get("/v1/notifications").with_session(session);
        async move {
            let body = request.await.json::<Value>();
            body["notifications"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|n| n["message"] == "Expedite the template")
                .cloned()
                .collect::<Vec<Value>>()
        }
    };
    let marks = bell(sessions::MANAGER).await;
    let titles: Vec<&str> = marks
        .iter()
        .map(|n| n["type_title"].as_str().unwrap())
        .collect();
    assert!(titles.contains(&"Gave You a Task"), "{marks:?}");
    assert!(titles.contains(&"Task Due"), "{marks:?}");
    let href = format!("/employee/projects/edit/{id}");
    assert!(marks.iter().all(|n| n["href"] == href.as_str()));
    // Ada gave it; she gets nothing.
    assert!(bell(sessions::ADMIN).await.is_empty());

    // Done: the due reminder leaves the bell.
    server
        .patch(&format!("/v1/projects/{id}/tasks/{task}"))
        .with_session(sessions::MANAGER)
        .json(&json!({ "completed": true }))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    let after = bell(sessions::MANAGER).await;
    assert!(
        after.iter().all(|n| n["type_title"] != "Task Due"),
        "{after:?}"
    );

    // Only its maker (Ada) or a manager deletes it.
    server
        .delete(&format!("/v1/projects/{id}/tasks/{task}"))
        .with_session(sessions::REP)
        .await
        .assert_status(StatusCode::FORBIDDEN);
}

#[test_db::test(seed)]
async fn payment_is_confirmed_once_and_a_stale_page_is_told(pool: MySqlPool) {
    turn_on(&pool).await;
    win_8002(&pool).await;
    let id = projects_of_8002(&pool).await[0];
    run(
        &pool,
        "INSERT INTO sales (id, customer_id, seller_id, company_id, price, status, project_address, sale_date)
         VALUES (7100, 1004, 100, 100, 500.00, 'pending', '5 Oak Ave', NOW())",
    )
    .await;
    let stale = version(&pool, id).await;
    let linked = act(
        &pool,
        id,
        sessions::ADMIN,
        json!({ "intent": "link-sale", "version": stale, "saleId": 7100 }),
    )
    .await;
    linked.assert_status_ok();
    assert_eq!(linked.json::<Value>()["sale_id"], 7100);

    // The page still had the old version.
    act(
        &pool,
        id,
        sessions::ADMIN,
        json!({ "intent": "paid", "version": stale, "date": "2026-01-05" }),
    )
    .await
    .assert_status(StatusCode::CONFLICT);

    let paid =
        json!({ "intent": "paid", "version": version(&pool, id).await, "date": "2026-01-05" });
    act(&pool, id, sessions::ADMIN, paid)
        .await
        .assert_status_ok();
    let again = act(
        &pool,
        id,
        sessions::ADMIN,
        json!({ "intent": "paid", "version": version(&pool, id).await, "date": "2026-01-06" }),
    )
    .await;
    again.assert_status(StatusCode::CONFLICT);
    let error = again.json::<Value>()["error"].as_str().unwrap().to_string();
    assert!(error.contains("already confirmed"), "{error}");

    // A sales rep doesn't move stages.
    act(
        &pool,
        id,
        sessions::REP,
        json!({ "intent": "stage", "version": version(&pool, id).await, "stage": "templated" }),
    )
    .await
    .assert_status(StatusCode::FORBIDDEN);
}
