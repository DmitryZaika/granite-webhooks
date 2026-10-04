mod common;

use api::auth::cookie::SessionData;
use common::{WithSession, cookie_for, server, sessions};
use serde_json::json;
use sqlx::MySqlPool;

#[sqlx::test(
    migrations = "../migrations",
    fixtures(path = "../seed", scripts("local_seed"))
)]
async fn me_returns_session_user(pool: MySqlPool) {
    let res = server(pool).get("/v1/me").with_session(sessions::REP).await;
    res.assert_status_ok();
    res.assert_json(&json!({
        "id": 100,
        "email": "rep@acme.test",
        "name": "Rita Rep",
        "phone_number": null,
        "is_employee": true,
        "is_admin": false,
        "is_superuser": false,
        "company_id": 100,
        "pined_bar": false,
        "cloudtalk_agent_id": null,
        "cloudtalk_phone_number": null,
        "ringcentral_extension_id": null,
        "ringcentral_phone_number": null,
        "is_signature_available": false
    }));
}

#[sqlx::test(
    migrations = "../migrations",
    fixtures(path = "../seed", scripts("local_seed"))
)]
async fn positions_are_scoped_to_the_acting_company(pool: MySqlPool) {
    let server = server(pool);

    // Manager holds sales_manager in 100 and sales_rep in 101.
    server
        .get("/v1/me/positions")
        .with_session(sessions::MANAGER)
        .await
        .assert_json(&json!([{ "position_id": 2 }]));

    server
        .get("/v1/me/positions")
        .with_session(sessions::VIEWER)
        .await
        .assert_json(&json!([]));

    let super_in_100 = cookie_for(SessionData {
        session_id: Some(sessions::SUPER_ADMIN.into()),
        active_company_id: Some(100),
    });
    server
        .get("/v1/me/positions")
        .add_header("cookie", super_in_100)
        .await
        .assert_json(&json!([{ "position_id": 9 }]));
}

#[sqlx::test(
    migrations = "../migrations",
    fixtures(path = "../seed", scripts("local_seed"))
)]
async fn positions_require_a_session(pool: MySqlPool) {
    server(pool)
        .get("/v1/me/positions")
        .await
        .assert_status_unauthorized();
}
