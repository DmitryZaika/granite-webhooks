use crate::crud::deals::find_customer_by_phone;
use crate::crud::leads::get_existing_deal;
use crate::telnyx::schemas::TelnyxEvent;
use lambda_http::tracing;
use sqlx::MySqlPool;
use sqlx::mysql::MySqlQueryResult;

/// Create or enrich the call-level columns. Called for `CallStatus=completed`.
pub async fn upsert_call(
    pool: &MySqlPool,
    event: &TelnyxEvent,
) -> Result<MySqlQueryResult, sqlx::Error> {
    sqlx::query(
        r#"
        INSERT INTO telnyx_calls
            (account_sid, call_sid, call_sid_legacy, call_session_id, call_control_id, call_leg_id,
             connection_id, callback_source, call_status, from_number, to_number, caller_id,
             hangup_cause, hangup_source, sip_hangup_cause, call_duration, dial_call_duration,
             start_time, end_time, answered_time, call_initiated_at, timestamp, occurred_at,
             sequence_number)
        VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
        ON DUPLICATE KEY UPDATE
            account_sid = VALUES(account_sid),
            call_sid_legacy = VALUES(call_sid_legacy),
            call_session_id = VALUES(call_session_id),
            call_control_id = VALUES(call_control_id),
            call_leg_id = VALUES(call_leg_id),
            connection_id = VALUES(connection_id),
            callback_source = VALUES(callback_source),
            call_status = VALUES(call_status),
            from_number = VALUES(from_number),
            to_number = VALUES(to_number),
            caller_id = VALUES(caller_id),
            hangup_cause = VALUES(hangup_cause),
            hangup_source = VALUES(hangup_source),
            sip_hangup_cause = VALUES(sip_hangup_cause),
            call_duration = VALUES(call_duration),
            dial_call_duration = VALUES(dial_call_duration),
            start_time = VALUES(start_time),
            end_time = VALUES(end_time),
            answered_time = VALUES(answered_time),
            call_initiated_at = VALUES(call_initiated_at),
            timestamp = VALUES(timestamp),
            occurred_at = VALUES(occurred_at),
            sequence_number = VALUES(sequence_number)
        "#,
    )
    .bind(event.account_sid.as_deref())
    .bind(event.call_sid.as_deref())
    .bind(event.call_sid_legacy.as_deref())
    .bind(event.call_session_id.as_deref())
    .bind(event.call_control_id.as_deref())
    .bind(event.call_leg_id.as_deref())
    .bind(event.connection_id.as_deref())
    .bind(event.callback_source.as_deref())
    .bind(event.call_status.as_deref())
    .bind(event.from.as_deref())
    .bind(event.to.as_deref())
    .bind(event.caller_id.as_deref())
    .bind(event.hangup_cause.as_deref())
    .bind(event.hangup_source.as_deref())
    .bind(event.sip_hangup_cause.as_deref())
    .bind(event.call_duration)
    .bind(event.dial_call_duration)
    .bind(event.start_time.as_deref())
    .bind(event.end_time.as_deref())
    .bind(event.answered_time.as_deref())
    .bind(event.call_initiated_at.as_deref())
    .bind(event.timestamp.as_deref())
    .bind(event.occurred_at.as_deref())
    .bind(event.sequence_number)
    .execute(pool)
    .await
}

/// Create or enrich the recording columns. Called for `RecordingStatus=completed`.
pub async fn upsert_recording(
    pool: &MySqlPool,
    event: &TelnyxEvent,
) -> Result<MySqlQueryResult, sqlx::Error> {
    sqlx::query(
        r#"
        INSERT INTO telnyx_calls
            (call_sid, recording_sid, recording_status, recording_source, recording_channels,
             recording_duration, recording_start_time, recording_end_time, recording_url)
        VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
        ON DUPLICATE KEY UPDATE
            recording_sid = VALUES(recording_sid),
            recording_status = VALUES(recording_status),
            recording_source = VALUES(recording_source),
            recording_channels = VALUES(recording_channels),
            recording_duration = VALUES(recording_duration),
            recording_start_time = VALUES(recording_start_time),
            recording_end_time = VALUES(recording_end_time),
            recording_url = VALUES(recording_url)
        "#,
    )
    .bind(event.call_sid.as_deref())
    .bind(event.recording_sid.as_deref())
    .bind(event.recording_status.as_deref())
    .bind(event.recording_source.as_deref())
    .bind(event.recording_channels)
    .bind(event.recording_duration)
    .bind(event.recording_start_time.as_deref())
    .bind(event.recording_end_time.as_deref())
    .bind(event.recording_url.as_deref())
    .execute(pool)
    .await
}

/// Create or enrich the transcript/conversation columns. Called for
/// `CallStatus=conversation_ended`.
pub async fn upsert_conversation(
    pool: &MySqlPool,
    event: &TelnyxEvent,
    messages_json: Option<String>,
    transcript: Option<String>,
    raw_payload: String,
) -> Result<MySqlQueryResult, sqlx::Error> {
    sqlx::query(
        r#"
        INSERT INTO telnyx_calls
            (call_sid, conversation_id, assistant_id, duration_sec, llm_model, stt_model,
             tts_model_id, tts_provider, tts_voice_id, reason, messages_json, transcript,
             raw_payload)
        VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
        ON DUPLICATE KEY UPDATE
            conversation_id = VALUES(conversation_id),
            assistant_id = VALUES(assistant_id),
            duration_sec = VALUES(duration_sec),
            llm_model = VALUES(llm_model),
            stt_model = VALUES(stt_model),
            tts_model_id = VALUES(tts_model_id),
            tts_provider = VALUES(tts_provider),
            tts_voice_id = VALUES(tts_voice_id),
            reason = VALUES(reason),
            messages_json = VALUES(messages_json),
            transcript = VALUES(transcript),
            raw_payload = VALUES(raw_payload)
        "#,
    )
    .bind(event.call_sid.as_deref())
    .bind(event.conversation_id.as_deref())
    .bind(event.assistant_id.as_deref())
    .bind(event.duration_sec)
    .bind(event.llm_model.as_deref())
    .bind(event.stt_model.as_deref())
    .bind(event.tts_model_id.as_deref())
    .bind(event.tts_provider.as_deref())
    .bind(event.tts_voice_id.as_deref())
    .bind(event.reason.as_deref())
    .bind(messages_json)
    .bind(transcript)
    .bind(raw_payload)
    .execute(pool)
    .await
}

#[derive(Debug)]
pub struct TelnyxCallLookup {
    pub id: i32,
    pub from_number: Option<String>,
}

/// Load the stored call row for a `call_sid`. Used after an upsert so we can
/// reference the conversation message row (`telnyx_calls.id`) and read the
/// caller's phone number even when this event did not carry a `From` field.
pub async fn find_call_by_sid(
    pool: &MySqlPool,
    call_sid: &str,
) -> Result<Option<TelnyxCallLookup>, sqlx::Error> {
    sqlx::query_as!(
        TelnyxCallLookup,
        "SELECT id, from_number FROM telnyx_calls WHERE call_sid = ?",
        call_sid,
    )
    .fetch_optional(pool)
    .await
}

/// Reduce a phone number to its last 10 digits, ignoring any formatting.
pub fn last10_digits(phone: &str) -> String {
    let digits: String = phone.chars().filter(|c| c.is_ascii_digit()).collect();
    let start = digits.len().saturating_sub(10);
    digits[start..].to_string()
}

fn note_content(phone: &str, transcript: &str) -> String {
    if transcript.is_empty() {
        format!("Inbound call from {phone}")
    } else {
        format!("Inbound call from {phone}:\n\n{transcript}")
    }
}

async fn note_exists(pool: &MySqlPool, deal_id: u64, content: &str) -> Result<bool, sqlx::Error> {
    let id = sqlx::query_scalar!(
        "SELECT id FROM deal_notes WHERE deal_id = ? AND created_by = 'telnyx' AND content = ? LIMIT 1",
        deal_id,
        content,
    )
    .fetch_optional(pool)
    .await?;
    Ok(id.is_some())
}

/// Add a Telnyx transcript note to a deal, deduplicating on identical content
/// so webhook retries do not create duplicate notes.
pub async fn add_telnyx_note(
    pool: &MySqlPool,
    deal_id: u64,
    company_id: i32,
    content: &str,
) -> Result<(), sqlx::Error> {
    if note_exists(pool, deal_id, content).await? {
        return Ok(());
    }

    sqlx::query!(
        "INSERT INTO deal_notes (deal_id, company_id, content, created_by) VALUES (?, ?, ?, 'telnyx')",
        deal_id,
        company_id,
        content,
    )
    .execute(pool)
    .await?;

    Ok(())
}

/// Create a follow-up for an unrecognized caller. One follow-up per call.
pub async fn insert_follow_up(pool: &MySqlPool, telnyx_call_id: i32) -> Result<(), sqlx::Error> {
    sqlx::query!(
        "INSERT IGNORE INTO follow_ups (telnyx_call_id, created_at) VALUES (?, NOW())",
        telnyx_call_id,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Mark a follow-up as reviewed by setting `marked_at`.
pub async fn mark_follow_up_reviewed(
    pool: &MySqlPool,
    follow_up_id: i32,
) -> Result<(), sqlx::Error> {
    sqlx::query!(
        "UPDATE follow_ups SET marked_at = NOW() WHERE id = ?",
        follow_up_id,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// After a call's transcript is stored, either attach it to the caller's most
/// recent deal (known phone number) or create a follow-up (unknown phone
/// number). Runs once per `conversation_ended` event and is idempotent.
pub async fn record_inbound_follow_up(
    pool: &MySqlPool,
    event: &TelnyxEvent,
    transcript: Option<&str>,
) -> Result<(), sqlx::Error> {
    let Some(call_sid) = event.call_sid.as_deref() else {
        return Ok(());
    };

    let Some(call) = find_call_by_sid(pool, call_sid).await? else {
        return Ok(());
    };

    let phone = call
        .from_number
        .as_deref()
        .or(event.from.as_deref())
        .map(str::trim)
        .filter(|phone| !phone.is_empty());

    let Some(phone) = phone else {
        tracing::warn!(
            call_sid,
            "Telnyx call has no caller phone; skipping follow-up"
        );
        return Ok(());
    };

    let last10 = last10_digits(phone);
    if last10.is_empty() {
        tracing::warn!(call_sid, phone, "Telnyx caller phone contained no digits");
        return Ok(());
    }

    let transcript = transcript.unwrap_or_default().trim();

    let customer = find_customer_by_phone(pool, &last10).await?;
    let deal = match &customer {
        Some(customer) => get_existing_deal(pool, customer.customer_id).await?,
        None => None,
    };

    match (customer, deal) {
        // `deal_notes.company_id` is NOT NULL, so a customer without a company
        // cannot receive a note; fall back to a follow-up so the call is not lost.
        (Some(customer), Some(deal)) => match customer.company_id {
            Some(company_id) => {
                add_telnyx_note(pool, deal.id, company_id, &note_content(phone, transcript))
                    .await?;
            }
            None => {
                tracing::warn!(
                    call_sid,
                    customer_id = customer.customer_id,
                    "Telnyx caller's customer has no company; creating follow-up instead of note"
                );
                insert_follow_up(pool, call.id).await?;
            }
        },
        _ => {
            insert_follow_up(pool, call.id).await?;
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDateTime;
    use sqlx::MySqlPool;

    fn event_for(call_sid: &str, from: &str) -> TelnyxEvent {
        TelnyxEvent {
            call_sid: Some(call_sid.to_string()),
            from: Some(from.to_string()),
            ..TelnyxEvent::default()
        }
    }

    async fn insert_company(pool: &MySqlPool) -> i32 {
        sqlx::query!("INSERT INTO company (name) VALUES ('Telnyx Test')")
            .execute(pool)
            .await
            .unwrap()
            .last_insert_id() as i32
    }

    async fn insert_customer(pool: &MySqlPool, company_id: i32, phone: Option<&str>) -> i32 {
        sqlx::query!(
            "INSERT INTO customers (name, company_id, phone, source) VALUES ('Caller', ?, ?, 'leads')",
            company_id,
            phone,
        )
        .execute(pool)
        .await
        .unwrap()
        .last_insert_id() as i32
    }

    async fn insert_deal(pool: &MySqlPool, customer_id: i32) -> u64 {
        sqlx::query!(
            "INSERT INTO deals (customer_id, status, list_id, position) VALUES (?, 'New Customer', 1, 0)",
            customer_id,
        )
        .execute(pool)
        .await
        .unwrap()
        .last_insert_id()
    }

    async fn insert_telnyx_call(pool: &MySqlPool, call_sid: &str, from_number: &str) -> i32 {
        sqlx::query!(
            "INSERT INTO telnyx_calls (call_sid, from_number) VALUES (?, ?)",
            call_sid,
            from_number,
        )
        .execute(pool)
        .await
        .unwrap()
        .last_insert_id() as i32
    }

    struct FollowUpRow {
        created_at: Option<NaiveDateTime>,
        marked_at: Option<NaiveDateTime>,
    }

    async fn follow_up_for_call(pool: &MySqlPool, telnyx_call_id: i32) -> Option<FollowUpRow> {
        sqlx::query_as!(
            FollowUpRow,
            "SELECT created_at, marked_at FROM follow_ups WHERE telnyx_call_id = ?",
            telnyx_call_id,
        )
        .fetch_optional(pool)
        .await
        .unwrap()
    }

    async fn count_follow_ups(pool: &MySqlPool) -> i64 {
        sqlx::query_scalar!("SELECT COUNT(*) FROM follow_ups")
            .fetch_one(pool)
            .await
            .unwrap()
    }

    async fn count_telnyx_notes(pool: &MySqlPool) -> i64 {
        sqlx::query_scalar!("SELECT COUNT(*) FROM deal_notes WHERE created_by = 'telnyx'")
            .fetch_one(pool)
            .await
            .unwrap()
    }

    #[test]
    fn strips_phone_formatting_to_last10() {
        assert_eq!(last10_digits("+1 (317) 316-1456"), "3173161456");
        assert_eq!(last10_digits("+13173161456"), "3173161456");
        assert_eq!(last10_digits("61456"), "61456");
        assert_eq!(last10_digits(""), "");
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn unknown_number_creates_unreviewed_follow_up(pool: MySqlPool) {
        let call_id = insert_telnyx_call(&pool, "call-unknown", "+15551234567").await;

        record_inbound_follow_up(
            &pool,
            &event_for("call-unknown", "+15551234567"),
            Some("Hello"),
        )
        .await
        .unwrap();

        let follow_up = follow_up_for_call(&pool, call_id).await.unwrap();
        assert!(follow_up.created_at.is_some());
        assert!(follow_up.marked_at.is_none());
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn known_number_adds_note_to_most_recent_deal(pool: MySqlPool) {
        let company_id = insert_company(&pool).await;
        let customer_id = insert_customer(&pool, company_id, Some("+13173161456")).await;
        let deal_id = insert_deal(&pool, customer_id).await;

        insert_telnyx_call(&pool, "call-known", "+13173161456").await;

        record_inbound_follow_up(
            &pool,
            &event_for("call-known", "+13173161456"),
            Some("Hi there"),
        )
        .await
        .unwrap();

        let content = sqlx::query_scalar!(
            "SELECT content FROM deal_notes WHERE deal_id = ? AND created_by = 'telnyx'",
            deal_id,
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(content.contains("Inbound call from +13173161456"));
        assert!(content.contains("Hi there"));

        assert_eq!(count_follow_ups(&pool).await, 0);
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn known_number_without_deal_creates_follow_up(pool: MySqlPool) {
        let company_id = insert_company(&pool).await;
        insert_customer(&pool, company_id, Some("+13173161456")).await;

        let call_id = insert_telnyx_call(&pool, "call-nodeal", "+13173161456").await;

        record_inbound_follow_up(&pool, &event_for("call-nodeal", "+13173161456"), Some("Hi"))
            .await
            .unwrap();

        assert!(follow_up_for_call(&pool, call_id).await.is_some());
        assert_eq!(count_telnyx_notes(&pool).await, 0);
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn repeated_events_do_not_duplicate_follow_up(pool: MySqlPool) {
        insert_telnyx_call(&pool, "call-retry", "+15551234567").await;
        let event = event_for("call-retry", "+15551234567");

        record_inbound_follow_up(&pool, &event, Some("Hello"))
            .await
            .unwrap();
        record_inbound_follow_up(&pool, &event, Some("Hello"))
            .await
            .unwrap();

        assert_eq!(count_follow_ups(&pool).await, 1);
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn repeated_events_do_not_duplicate_note(pool: MySqlPool) {
        let company_id = insert_company(&pool).await;
        let customer_id = insert_customer(&pool, company_id, Some("+13173161456")).await;
        insert_deal(&pool, customer_id).await;

        insert_telnyx_call(&pool, "call-note-retry", "+13173161456").await;
        let event = event_for("call-note-retry", "+13173161456");

        record_inbound_follow_up(&pool, &event, Some("Hi there"))
            .await
            .unwrap();
        record_inbound_follow_up(&pool, &event, Some("Hi there"))
            .await
            .unwrap();

        assert_eq!(count_telnyx_notes(&pool).await, 1);
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn marking_follow_up_sets_marked_at(pool: MySqlPool) {
        let call_id = insert_telnyx_call(&pool, "call-mark", "+15551234567").await;
        record_inbound_follow_up(
            &pool,
            &event_for("call-mark", "+15551234567"),
            Some("Hello"),
        )
        .await
        .unwrap();

        let follow_up_id = sqlx::query_scalar!(
            "SELECT id FROM follow_ups WHERE telnyx_call_id = ?",
            call_id,
        )
        .fetch_one(&pool)
        .await
        .unwrap();

        mark_follow_up_reviewed(&pool, follow_up_id).await.unwrap();

        let follow_up = follow_up_for_call(&pool, call_id).await.unwrap();
        assert!(follow_up.marked_at.is_some());
    }
}
