//! The call webhooks move a lead's deal off "Not Contacted Yet" (`crud::deals`).

use crate::axum_helpers::guards::CORRECT_ID;
use crate::tests::deals::{deal_list_id, setup_board};
use crate::tests::utils::new_test_app;
use axum::http::StatusCode;
use sqlx::MySqlPool;

/// Replays a short outgoing leg logged alongside an inbound call: the leg is
/// ignored and the inbound call still moves the deal.
async fn assert_call_webhooks_move_deal(pool: MySqlPool, provider: &str) {
    let board = setup_board(&pool, Some("555-771-1113")).await;
    let app = new_test_app(pool.clone());
    let route = format!("/{provider}/call/{}", board.company_id);

    let outgoing_leg = serde_json::json!({
        "Cdr": { "public_external": "+15557711113", "type": "outgoing", "talking_time": "1", "id": "1" }
    });
    let response = app
        .post(&route)
        .authorization_bearer(CORRECT_ID.to_string())
        .json(&outgoing_leg)
        .await;
    assert_eq!(response.status_code(), StatusCode::OK);
    assert_eq!(
        deal_list_id(&pool, board.deal_id).await,
        board.first_list_id
    );

    let incoming = serde_json::json!({
        "Cdr": { "public_external": "+15557711113", "type": "incoming", "talking_time": "343", "id": "2" }
    });
    let response = app
        .post(&route)
        .authorization_bearer(CORRECT_ID.to_string())
        .json(&incoming)
        .await;
    assert_eq!(response.status_code(), StatusCode::OK);
    assert_eq!(
        deal_list_id(&pool, board.deal_id).await,
        board.second_list_id
    );
}

#[test_db::test]
async fn cloudtalk_inbound_call_webhook_moves_deal(pool: MySqlPool) {
    assert_call_webhooks_move_deal(pool, "cloudtalk").await;
}

#[test_db::test]
async fn ringcentral_inbound_call_webhook_moves_deal(pool: MySqlPool) {
    assert_call_webhooks_move_deal(pool, "ringcentral").await;
}
