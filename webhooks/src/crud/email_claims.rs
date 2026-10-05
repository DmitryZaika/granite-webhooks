//! Claims on inbound emails, so overlapping `EventBridge` deliveries of one
//! email do not both run the slow pipeline (table `email_processing_claims`).

use lambda_http::tracing;
use sqlx::MySqlPool;
use sqlx::mysql::MySqlDatabaseError;

/// A claim older than this is treated as abandoned.
///
/// It must exceed the 30 s Lambda timeout (by then the invocation that wrote
/// it is certainly gone),
/// and stays under the ~56 s of `EventBridge`'s second retry, so that retry
/// can take over from a first run the timeout killed.
pub const EMAIL_CLAIM_TTL_SECONDS: i64 = 40;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmailClaim {
    /// This invocation holds the claim and must release it when done.
    Acquired,
    /// Another live invocation is processing this email.
    HeldByAnother,
    /// No claim could be taken (table missing, DB error). Process anyway,
    /// as before the claims existed.
    Unavailable,
}

/// Claims `message_id` for this invocation (`token`, a fresh UUID).
///
/// One statement either inserts the claim, takes over a claim older than
/// [`EMAIL_CLAIM_TTL_SECONDS`], or leaves a live claim untouched. The result
/// is read back by token instead of from the affected-row count, which
/// `CLIENT_FOUND_ROWS` changes. Any DB failure, including a missing table
/// (migration not applied yet), is logged and returns
/// [`EmailClaim::Unavailable`] so intake never depends on this table.
pub async fn claim_email_processing(pool: &MySqlPool, message_id: &str, token: &str) -> EmailClaim {
    match try_claim(pool, message_id, token).await {
        Ok(true) => EmailClaim::Acquired,
        Ok(false) => EmailClaim::HeldByAnother,
        Err(error) => {
            if is_missing_table(&error) {
                tracing::warn!("email_processing_claims table missing; processing without a claim");
            } else {
                tracing::warn!(
                    ?error,
                    "Failed to claim inbound email; processing without a claim"
                );
            }
            EmailClaim::Unavailable
        }
    }
}

async fn try_claim(pool: &MySqlPool, message_id: &str, token: &str) -> Result<bool, sqlx::Error> {
    // ON DUPLICATE KEY UPDATE assigns left to right, and each assignment sees
    // the columns as already updated. So `claimed_at` is refreshed exactly
    // when `claim_token` now holds our token (an expired claim was taken
    // over); a live claim keeps both its token and its time.
    sqlx::query(
        r"
        INSERT INTO email_processing_claims (message_id, claim_token, claimed_at)
        VALUES (?, ?, NOW(3))
        ON DUPLICATE KEY UPDATE
            claim_token = IF(claimed_at < NOW(3) - INTERVAL ? SECOND,
                             VALUES(claim_token), claim_token),
            claimed_at = IF(claim_token = VALUES(claim_token), NOW(3), claimed_at)
        ",
    )
    .bind(message_id)
    .bind(token)
    .bind(EMAIL_CLAIM_TTL_SECONDS)
    .execute(pool)
    .await?;
    let holder: Option<String> =
        sqlx::query_scalar("SELECT claim_token FROM email_processing_claims WHERE message_id = ?")
            .bind(message_id)
            .fetch_optional(pool)
            .await?;
    Ok(holder.as_deref() == Some(token))
}

/// Deletes the claim if it is still ours. A claim taken over after expiry
/// belongs to the newer invocation and is left alone.
pub async fn release_email_processing(
    pool: &MySqlPool,
    message_id: &str,
    token: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM email_processing_claims WHERE message_id = ? AND claim_token = ?")
        .bind(message_id)
        .bind(token)
        .execute(pool)
        .await
        .map(|_| ())
}

/// MySQL error 1146: table doesn't exist.
fn is_missing_table(error: &sqlx::Error) -> bool {
    error
        .as_database_error()
        .and_then(|db_error| db_error.try_downcast_ref::<MySqlDatabaseError>())
        .is_some_and(|db_error| db_error.number() == 1146)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MESSAGE_ID: &str = "claim-test-1@mail.example.net";
    const TOKEN_A: &str = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
    const TOKEN_B: &str = "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb";

    async fn holder(pool: &MySqlPool, message_id: &str) -> Option<String> {
        sqlx::query_scalar("SELECT claim_token FROM email_processing_claims WHERE message_id = ?")
            .bind(message_id)
            .fetch_optional(pool)
            .await
            .unwrap()
    }

    async fn age_claim(pool: &MySqlPool, message_id: &str, seconds: i64) {
        sqlx::query(
            "UPDATE email_processing_claims \
             SET claimed_at = claimed_at - INTERVAL ? SECOND WHERE message_id = ?",
        )
        .bind(seconds)
        .bind(message_id)
        .execute(pool)
        .await
        .unwrap();
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn first_claim_wins(pool: MySqlPool) {
        assert_eq!(
            claim_email_processing(&pool, MESSAGE_ID, TOKEN_A).await,
            EmailClaim::Acquired
        );
        assert_eq!(holder(&pool, MESSAGE_ID).await.as_deref(), Some(TOKEN_A));
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn second_claim_with_another_token_loses(pool: MySqlPool) {
        assert_eq!(
            claim_email_processing(&pool, MESSAGE_ID, TOKEN_A).await,
            EmailClaim::Acquired
        );
        assert_eq!(
            claim_email_processing(&pool, MESSAGE_ID, TOKEN_B).await,
            EmailClaim::HeldByAnother
        );
        assert_eq!(holder(&pool, MESSAGE_ID).await.as_deref(), Some(TOKEN_A));
        // A different email is not blocked by this claim.
        assert_eq!(
            claim_email_processing(&pool, "other@mail.example.net", TOKEN_B).await,
            EmailClaim::Acquired
        );
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn live_claim_is_not_taken_over_and_keeps_its_time(pool: MySqlPool) {
        claim_email_processing(&pool, MESSAGE_ID, TOKEN_A).await;
        age_claim(&pool, MESSAGE_ID, EMAIL_CLAIM_TTL_SECONDS - 5).await;
        assert_eq!(
            claim_email_processing(&pool, MESSAGE_ID, TOKEN_B).await,
            EmailClaim::HeldByAnother
        );
        // The losing attempt must not refresh claimed_at, or a dead holder's
        // claim would never expire while retries keep arriving.
        let fresh: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM email_processing_claims \
             WHERE message_id = ? AND claimed_at < NOW(3) - INTERVAL ? SECOND",
        )
        .bind(MESSAGE_ID)
        .bind(EMAIL_CLAIM_TTL_SECONDS - 10)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(fresh, 1);
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn expired_claim_is_taken_over(pool: MySqlPool) {
        claim_email_processing(&pool, MESSAGE_ID, TOKEN_A).await;
        // 41 s: a claim left by a run the 30 s Lambda timeout killed is free
        // again by the second EventBridge retry (about 56 s after the first).
        age_claim(&pool, MESSAGE_ID, 41).await;
        assert_eq!(
            claim_email_processing(&pool, MESSAGE_ID, TOKEN_B).await,
            EmailClaim::Acquired
        );
        assert_eq!(holder(&pool, MESSAGE_ID).await.as_deref(), Some(TOKEN_B));
        // The takeover restarts the clock: the old holder cannot win it back.
        assert_eq!(
            claim_email_processing(&pool, MESSAGE_ID, TOKEN_A).await,
            EmailClaim::HeldByAnother
        );
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn release_deletes_only_your_own_claim(pool: MySqlPool) {
        claim_email_processing(&pool, MESSAGE_ID, TOKEN_A).await;
        release_email_processing(&pool, MESSAGE_ID, TOKEN_B)
            .await
            .unwrap();
        assert_eq!(holder(&pool, MESSAGE_ID).await.as_deref(), Some(TOKEN_A));

        release_email_processing(&pool, MESSAGE_ID, TOKEN_A)
            .await
            .unwrap();
        assert_eq!(holder(&pool, MESSAGE_ID).await, None);
        // Once released, the next delivery can claim it at once.
        assert_eq!(
            claim_email_processing(&pool, MESSAGE_ID, TOKEN_B).await,
            EmailClaim::Acquired
        );
    }

    /// The Lambda can be deployed before the migration is applied: then every
    /// email is processed without a claim, as before.
    #[sqlx::test(migrations = "../migrations")]
    async fn missing_table_degrades_to_no_claim(pool: MySqlPool) {
        sqlx::query("DROP TABLE email_processing_claims")
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(
            claim_email_processing(&pool, MESSAGE_ID, TOKEN_A).await,
            EmailClaim::Unavailable
        );
    }
}
