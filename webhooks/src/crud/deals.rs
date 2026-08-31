use common::crud::scheduled_emails::{
    cancel_pending_scheduled_emails_for_deal, reschedule_templates_for_deal_list,
};
use lambda_http::tracing;
use sqlx::MySqlPool;

use crate::amazonses::parse_email::normalize_address;
use crate::crud::cloudtalk::cancel_flow_enrollments_for_customer;
use crate::crud::email::{SendEmail, get_inbound_email_notify_context};
use crate::crud::leads::get_existing_deal;

struct DealMoveContext {
    id: u64,
    customer_id: i32,
    user_id: Option<i32>,
    list_id: i32,
    company_id: Option<i32>,
    group_id: i32,
    list_position: i32,
}

struct NextDealList {
    id: i32,
    name: String,
}

async fn load_deal_move_context(
    pool: &MySqlPool,
    deal_id: u64,
) -> Result<Option<DealMoveContext>, sqlx::Error> {
    sqlx::query_as!(
        DealMoveContext,
        r#"
        SELECT
            d.id,
            d.customer_id,
            d.user_id,
            d.list_id,
            c.company_id,
            dl.group_id,
            dl.position AS list_position
        FROM deals d
        INNER JOIN customers c ON c.id = d.customer_id
        INNER JOIN deals_list dl ON dl.id = d.list_id AND dl.deleted_at IS NULL
        WHERE d.id = ?
          AND d.deleted_at IS NULL
        "#,
        deal_id
    )
    .fetch_optional(pool)
    .await
}

async fn first_list_id_in_group(
    pool: &MySqlPool,
    group_id: i32,
) -> Result<Option<i32>, sqlx::Error> {
    sqlx::query_scalar!(
        r#"
        SELECT id
        FROM deals_list
        WHERE group_id = ?
          AND deleted_at IS NULL
        ORDER BY position ASC, id ASC
        LIMIT 1
        "#,
        group_id
    )
    .fetch_optional(pool)
    .await
}

async fn next_list_in_group(
    pool: &MySqlPool,
    deal: &DealMoveContext,
) -> Result<Option<NextDealList>, sqlx::Error> {
    sqlx::query_as!(
        NextDealList,
        r#"
        SELECT id, name
        FROM deals_list
        WHERE group_id = ?
          AND deleted_at IS NULL
          AND (position > ? OR (position = ? AND id > ?))
        ORDER BY position ASC, id ASC
        LIMIT 1
        "#,
        deal.group_id,
        deal.list_position,
        deal.list_position,
        deal.list_id
    )
    .fetch_optional(pool)
    .await
}

async fn apply_deal_list_move(
    pool: &MySqlPool,
    deal: &DealMoveContext,
    next_list: &NextDealList,
) -> Result<(), sqlx::Error> {
    sqlx::query!(
        r#"
        UPDATE deals
        SET list_id = ?, status = ?, position = 0, is_won = NULL, lost_reason = NULL
        WHERE id = ? AND deleted_at IS NULL
        "#,
        next_list.id,
        next_list.name,
        deal.id
    )
    .execute(pool)
    .await?;

    sqlx::query!(
        r#"
        UPDATE deal_stage_history
        SET exited_at = NOW()
        WHERE deal_id = ? AND exited_at IS NULL
        "#,
        deal.id
    )
    .execute(pool)
    .await?;

    sqlx::query!(
        r#"
        INSERT INTO deal_stage_history (deal_id, list_id)
        VALUES (?, ?)
        "#,
        deal.id,
        next_list.id
    )
    .execute(pool)
    .await?;

    Ok(())
}

async fn reschedule_drip_after_move(
    pool: &MySqlPool,
    deal: &DealMoveContext,
    next_list: &NextDealList,
) {
    if let (Some(user_id), Some(company_id)) = (deal.user_id, deal.company_id) {
        if let Err(error) = reschedule_templates_for_deal_list(
            pool,
            next_list.id,
            company_id,
            deal.id,
            deal.customer_id,
            user_id,
        )
        .await
        {
            tracing::error!(
                ?error,
                deal_id = deal.id,
                list_id = next_list.id,
                "Failed to reschedule drip emails after moving deal to contacted"
            );
        }
    } else if let Err(error) = cancel_pending_scheduled_emails_for_deal(pool, deal.id).await {
        tracing::error!(
            ?error,
            deal_id = deal.id,
            "Failed to cancel pending drip emails after moving deal to contacted"
        );
    }
}

pub async fn move_deal_to_contacted_if_uncontacted(
    pool: &MySqlPool,
    deal_id: u64,
) -> Result<bool, sqlx::Error> {
    let Some(deal) = load_deal_move_context(pool, deal_id).await? else {
        return Ok(false);
    };
    let Some(first_list_id) = first_list_id_in_group(pool, deal.group_id).await? else {
        return Ok(false);
    };
    if deal.list_id != first_list_id {
        return Ok(false);
    }
    let Some(next_list) = next_list_in_group(pool, &deal).await? else {
        return Ok(false);
    };

    apply_deal_list_move(pool, &deal, &next_list).await?;
    reschedule_drip_after_move(pool, &deal, &next_list).await;
    Ok(true)
}

pub async fn find_customer_id_by_email(
    pool: &MySqlPool,
    company_id: i32,
    email: &str,
) -> Result<Option<i32>, sqlx::Error> {
    sqlx::query_scalar!(
        r#"
        SELECT c.id
        FROM customers c
        INNER JOIN customers_emails ce ON ce.customer_id = c.id
        WHERE c.company_id = ?
          AND c.deleted_at IS NULL
          AND LOWER(TRIM(SUBSTRING_INDEX(SUBSTRING_INDEX(ce.email, '<', -1), '>', 1))) = ?
        ORDER BY c.id DESC
        LIMIT 1
        "#,
        company_id,
        email
    )
    .fetch_optional(pool)
    .await
}

pub async fn find_customer_id_by_phone_last10(
    pool: &MySqlPool,
    company_id: i32,
    last10: &str,
) -> Result<Option<i32>, sqlx::Error> {
    if let Some(customer_id) = sqlx::query_scalar!(
        r#"
        SELECT c.id
        FROM customers c
        INNER JOIN cloudtalk_contacts cc ON cc.customer_id = c.id
        WHERE cc.company_id = ?
          AND c.deleted_at IS NULL
          AND (
            RIGHT(cc.phone_e164_1, 10) = ?
            OR RIGHT(cc.phone_e164_2, 10) = ?
          )
        ORDER BY c.id DESC
        LIMIT 1
        "#,
        company_id,
        last10,
        last10
    )
    .fetch_optional(pool)
    .await?
    {
        return Ok(Some(customer_id));
    }

    sqlx::query_scalar!(
        r#"
        SELECT c.id
        FROM customers c
        WHERE c.company_id = ?
          AND c.deleted_at IS NULL
          AND (
            RIGHT(REGEXP_REPLACE(COALESCE(c.phone, ''), '[^0-9]', ''), 10) = ?
            OR RIGHT(REGEXP_REPLACE(COALESCE(c.phone_2, ''), '[^0-9]', ''), 10) = ?
          )
        ORDER BY c.id DESC
        LIMIT 1
        "#,
        company_id,
        last10,
        last10
    )
    .fetch_optional(pool)
    .await
}

pub async fn move_deal_on_inbound_email(
    pool: &MySqlPool,
    send: &SendEmail,
) -> Result<bool, sqlx::Error> {
    let mut deal_id = None;

    if let Some(user_id) = send.receiver_user_id() {
        match get_inbound_email_notify_context(pool, send.thread_id(), user_id).await {
            Ok(Some(context)) => deal_id = context.deal_id,
            Ok(None) => {}
            Err(error) => {
                tracing::error!(
                    ?error,
                    thread_id = send.thread_id(),
                    "Failed to load inbound email deal context"
                );
            }
        }
    }

    if deal_id.is_none()
        && let Some(company_id) = send.company_id
    {
        let email = normalize_address(send.sender_email());
        if !email.is_empty()
            && let Some(customer_id) = find_customer_id_by_email(pool, company_id, &email).await?
            && let Some(deal) = get_existing_deal(pool, customer_id).await?
        {
            deal_id = Some(deal.id);
        }
    }

    let Some(deal_id) = deal_id else {
        return Ok(false);
    };
    move_deal_to_contacted_if_uncontacted(pool, deal_id).await
}

pub async fn maybe_move_deal_on_inbound_email(pool: &MySqlPool, send: &SendEmail) {
    if let Err(error) = move_deal_on_inbound_email(pool, send).await {
        tracing::error!(
            ?error,
            thread_id = send.thread_id(),
            "Failed to move deal to contacted on inbound email"
        );
    }
}

pub async fn cancel_flow_on_inbound_email(
    pool: &MySqlPool,
    send: &SendEmail,
) -> Result<u64, sqlx::Error> {
    let Some(company_id) = send.company_id else {
        return Ok(0);
    };
    let email = normalize_address(send.sender_email());
    if email.is_empty() {
        return Ok(0);
    }
    let Some(customer_id) = find_customer_id_by_email(pool, company_id, &email).await? else {
        return Ok(0);
    };
    cancel_flow_enrollments_for_customer(pool, company_id, customer_id).await
}

pub async fn maybe_cancel_flow_on_inbound_email(pool: &MySqlPool, send: &SendEmail) {
    if let Err(error) = cancel_flow_on_inbound_email(pool, send).await {
        tracing::error!(
            ?error,
            thread_id = send.thread_id(),
            "Failed to cancel sms flow on inbound email"
        );
    }
}

pub async fn move_deal_on_inbound_sms(
    pool: &MySqlPool,
    company_id: i32,
    sender: u64,
) -> Result<bool, sqlx::Error> {
    let last10 = sender.to_string();
    let Some(customer_id) = find_customer_id_by_phone_last10(pool, company_id, &last10).await?
    else {
        return Ok(false);
    };
    let Some(deal) = get_existing_deal(pool, customer_id).await? else {
        return Ok(false);
    };
    move_deal_to_contacted_if_uncontacted(pool, deal.id).await
}

pub async fn maybe_move_deal_on_inbound_sms(pool: &MySqlPool, company_id: i32, sender: u64) {
    if let Err(error) = move_deal_on_inbound_sms(pool, company_id, sender).await {
        tracing::error!(
            ?error,
            company_id,
            "Failed to move deal to contacted on inbound sms"
        );
    }
}

pub async fn maybe_move_deal_on_inbound_call(pool: &MySqlPool, company_id: i32, caller: u64) {
    if let Err(error) = move_deal_on_inbound_sms(pool, company_id, caller).await {
        tracing::error!(
            ?error,
            company_id,
            "Failed to move deal to contacted on inbound call"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::amazonses::parse_email::{ParsedEmail, ParsedRecipient};
    use crate::crud::email::SendEmail;
    use crate::crud::users::ReceivingEmail;
    use crate::tests::utils::{insert_group_list, insert_user};
    use common::crud::email_template::EmailTemplate;
    use common::crud::scheduled_emails::insert_scheduled_email;
    use sqlx::MySqlPool;
    use uuid::Uuid;

    struct BoardFixture {
        company_id: i32,
        user_id: i32,
        customer_id: i32,
        deal_id: u64,
        first_list_id: i32,
        second_list_id: i32,
        customer_email: String,
    }

    async fn insert_company(pool: &MySqlPool) -> i32 {
        let rec = sqlx::query!(r#"INSERT INTO company (name) VALUES ('Move Company')"#)
            .execute(pool)
            .await
            .unwrap();
        i32::try_from(rec.last_insert_id()).unwrap()
    }

    async fn insert_list(pool: &MySqlPool, group_id: u64, name: &str, position: i32) -> i32 {
        let rec = sqlx::query!(
            r#"INSERT INTO deals_list (name, group_id, position) VALUES (?, ?, ?)"#,
            name,
            group_id,
            position
        )
        .execute(pool)
        .await
        .unwrap();
        i32::try_from(rec.last_insert_id()).unwrap()
    }

    async fn setup_board(pool: &MySqlPool, phone: Option<&str>) -> BoardFixture {
        let company_id = insert_company(pool).await;
        let email = format!("rep_{}@example.com", Uuid::new_v4());
        let user_id = insert_user(pool, &email, None).await.unwrap();
        sqlx::query!(
            r#"UPDATE users SET company_id = ? WHERE id = ?"#,
            company_id,
            user_id
        )
        .execute(pool)
        .await
        .unwrap();

        let group_id = insert_group_list(pool, company_id).await.unwrap();
        let first_list_id = insert_list(pool, group_id, "Not Contacted Yet", 0).await;
        let second_list_id = insert_list(pool, group_id, "Contacted", 1).await;

        let customer_email = format!("lead_{}@example.com", Uuid::new_v4());
        let customer = sqlx::query!(
            r#"INSERT INTO customers (name, company_id, phone, source) VALUES ('Lead', ?, ?, 'leads')"#,
            company_id,
            phone
        )
        .execute(pool)
        .await
        .unwrap();
        let customer_id = i32::try_from(customer.last_insert_id()).unwrap();
        let email_row = sqlx::query!(
            r#"INSERT INTO customers_emails (customer_id, email) VALUES (?, ?)"#,
            customer_id,
            customer_email
        )
        .execute(pool)
        .await
        .unwrap();
        let email_id = i32::try_from(email_row.last_insert_id()).unwrap();
        sqlx::query!(
            r#"UPDATE customers SET email_id = ? WHERE id = ?"#,
            email_id,
            customer_id
        )
        .execute(pool)
        .await
        .unwrap();

        let deal = sqlx::query!(
            r#"INSERT INTO deals (customer_id, status, list_id, position, user_id) VALUES (?, 'Not Contacted Yet', ?, 0, ?)"#,
            customer_id,
            first_list_id,
            user_id
        )
        .execute(pool)
        .await
        .unwrap();

        sqlx::query!(
            r#"INSERT INTO deal_stage_history (deal_id, list_id) VALUES (?, ?)"#,
            deal.last_insert_id(),
            first_list_id
        )
        .execute(pool)
        .await
        .unwrap();

        BoardFixture {
            company_id,
            user_id,
            customer_id,
            deal_id: deal.last_insert_id(),
            first_list_id,
            second_list_id,
            customer_email,
        }
    }

    async fn deal_list_id(pool: &MySqlPool, deal_id: u64) -> i32 {
        sqlx::query_scalar!(r#"SELECT list_id FROM deals WHERE id = ?"#, deal_id)
            .fetch_one(pool)
            .await
            .unwrap()
    }

    fn parsed_email(sender: &str, receiver: &str) -> ParsedEmail {
        ParsedEmail {
            subject: Some("Re: hello".to_string()),
            body: "reply".to_string(),
            html_body: None,
            sender_email: sender.to_string(),
            receiver_email: receiver.to_string(),
            to_recipients: vec![ParsedRecipient {
                address: receiver.to_lowercase(),
                display_name: None,
            }],
            cc_recipients: vec![],
            bcc_recipients: vec![],
            forward_to_email: None,
            in_reply_to: None,
            references: vec![],
            message_id: format!("msg-{}", Uuid::new_v4()),
        }
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn moves_from_first_list_to_next(pool: MySqlPool) {
        let board = setup_board(&pool, Some("3173161456")).await;

        let moved = move_deal_to_contacted_if_uncontacted(&pool, board.deal_id)
            .await
            .unwrap();
        assert!(moved);
        assert_eq!(
            deal_list_id(&pool, board.deal_id).await,
            board.second_list_id
        );

        let status: Option<String> =
            sqlx::query_scalar!(r#"SELECT status FROM deals WHERE id = ?"#, board.deal_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(status.as_deref(), Some("Contacted"));

        let first_exited = sqlx::query_scalar!(
            r#"SELECT exited_at FROM deal_stage_history WHERE deal_id = ? AND list_id = ?"#,
            board.deal_id,
            board.first_list_id
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(first_exited.is_some());
        let second_exited = sqlx::query_scalar!(
            r#"SELECT exited_at FROM deal_stage_history WHERE deal_id = ? AND list_id = ?"#,
            board.deal_id,
            board.second_list_id
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(second_exited.is_none());
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn does_not_move_when_already_on_second_list(pool: MySqlPool) {
        let board = setup_board(&pool, Some("3173161456")).await;
        sqlx::query!(
            r#"UPDATE deals SET list_id = ? WHERE id = ?"#,
            board.second_list_id,
            board.deal_id
        )
        .execute(&pool)
        .await
        .unwrap();

        let moved = move_deal_to_contacted_if_uncontacted(&pool, board.deal_id)
            .await
            .unwrap();
        assert!(!moved);
        assert_eq!(
            deal_list_id(&pool, board.deal_id).await,
            board.second_list_id
        );
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn does_not_move_when_no_next_list(pool: MySqlPool) {
        let company_id = insert_company(&pool).await;
        let group_id = insert_group_list(&pool, company_id).await.unwrap();
        let only_list_id = insert_list(&pool, group_id, "Only", 0).await;
        let customer = sqlx::query!(
            r#"INSERT INTO customers (name, company_id, source) VALUES ('Solo', ?, 'leads')"#,
            company_id
        )
        .execute(&pool)
        .await
        .unwrap();
        let customer_id = i32::try_from(customer.last_insert_id()).unwrap();
        let deal = sqlx::query!(
            r#"INSERT INTO deals (customer_id, status, list_id, position) VALUES (?, 'Only', ?, 0)"#,
            customer_id,
            only_list_id
        )
        .execute(&pool)
        .await
        .unwrap();
        let deal_id = deal.last_insert_id();

        let moved = move_deal_to_contacted_if_uncontacted(&pool, deal_id)
            .await
            .unwrap();
        assert!(!moved);
        assert_eq!(deal_list_id(&pool, deal_id).await, only_list_id);
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn cancels_pending_drip_emails_on_move(pool: MySqlPool) {
        let board = setup_board(&pool, Some("3173161456")).await;
        let template = sqlx::query!(
            r#"INSERT INTO email_templates (template_name, template_body, company_id, hour_delay, show_template) VALUES ('drip', 'Hi', ?, 0, 1)"#,
            board.company_id
        )
        .execute(&pool)
        .await
        .unwrap();
        let template_id = i32::try_from(template.last_insert_id()).unwrap();
        insert_scheduled_email(
            &pool,
            EmailTemplate {
                id: template_id,
                hour_delay: Some(0),
            },
            board.deal_id,
            board.customer_id,
            board.user_id,
            board.company_id,
            Some(board.first_list_id),
        )
        .await
        .unwrap();

        move_deal_to_contacted_if_uncontacted(&pool, board.deal_id)
            .await
            .unwrap();

        let row: (String, Option<String>) = sqlx::query_as(
            "SELECT status, error_message FROM scheduled_emails WHERE deal_id = ? LIMIT 1",
        )
        .bind(board.deal_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(row.0, "cancelled");
        assert_eq!(row.1.as_deref(), Some("Deal moved lists"));
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn finds_customer_by_email_and_phone(pool: MySqlPool) {
        let board = setup_board(&pool, Some("(317) 316-1456")).await;

        let by_email = find_customer_id_by_email(&pool, board.company_id, &board.customer_email)
            .await
            .unwrap();
        assert_eq!(by_email, Some(board.customer_id));

        let by_phone = find_customer_id_by_phone_last10(&pool, board.company_id, "3173161456")
            .await
            .unwrap();
        assert_eq!(by_phone, Some(board.customer_id));
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn finds_customer_by_cloudtalk_contact_phone(pool: MySqlPool) {
        let board = setup_board(&pool, None).await;
        sqlx::query!(
            r#"INSERT INTO cloudtalk_contacts (customer_id, company_id, cloudtalk_id, phone_e164_1) VALUES (?, ?, 99, '+16468956758')"#,
            board.customer_id,
            board.company_id
        )
        .execute(&pool)
        .await
        .unwrap();

        let by_phone = find_customer_id_by_phone_last10(&pool, board.company_id, "6468956758")
            .await
            .unwrap();
        assert_eq!(by_phone, Some(board.customer_id));
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn inbound_email_moves_from_thread_deal_id(pool: MySqlPool) {
        let board = setup_board(&pool, Some("3173161456")).await;
        let thread_id = Uuid::new_v4().to_string();
        sqlx::query!(
            r#"INSERT INTO emails (subject, body, thread_id, receiver_user_id, sender_email, message_id, deal_id, company_id) VALUES ('Hi', 'body', ?, ?, ?, ?, ?, ?)"#,
            thread_id,
            board.user_id,
            board.customer_email,
            format!("orig-{}", Uuid::new_v4()),
            board.deal_id,
            board.company_id
        )
        .execute(&pool)
        .await
        .unwrap();

        let parsed = parsed_email(&board.customer_email, "rep@example.com");
        let send = SendEmail::new(
            &parsed,
            Some(thread_id),
            Some(ReceivingEmail::To(board.user_id)),
        )
        .with_company_id(Some(board.company_id));

        let moved = move_deal_on_inbound_email(&pool, &send).await.unwrap();
        assert!(moved);
        assert_eq!(
            deal_list_id(&pool, board.deal_id).await,
            board.second_list_id
        );
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn inbound_email_moves_from_sender_when_no_thread_deal(pool: MySqlPool) {
        let board = setup_board(&pool, Some("3173161456")).await;
        let parsed = parsed_email(
            &format!("Lead <{}>", board.customer_email),
            "rep@example.com",
        );
        let send = SendEmail::new(&parsed, None, Some(ReceivingEmail::To(board.user_id)))
            .with_company_id(Some(board.company_id));

        let moved = move_deal_on_inbound_email(&pool, &send).await.unwrap();
        assert!(moved);
        assert_eq!(
            deal_list_id(&pool, board.deal_id).await,
            board.second_list_id
        );
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn inbound_sms_moves_uncontacted_deal(pool: MySqlPool) {
        let board = setup_board(&pool, Some("646-895-6758")).await;
        let moved = move_deal_on_inbound_sms(&pool, board.company_id, 6_468_956_758)
            .await
            .unwrap();
        assert!(moved);
        assert_eq!(
            deal_list_id(&pool, board.deal_id).await,
            board.second_list_id
        );
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn inbound_email_cancels_sms_flow_for_customer(pool: MySqlPool) {
        let board = setup_board(&pool, Some("3173161456")).await;
        sqlx::query!(
            r#"
            INSERT INTO sms_flow_enrollments
                (flow_id, company_id, customer_phone_digits, customer_id, user_id, status, anchor_at)
            VALUES (1, ?, 5550000000, ?, 1, 'active', UTC_TIMESTAMP())
            "#,
            board.company_id,
            board.customer_id,
        )
        .execute(&pool)
        .await
        .unwrap();

        let parsed = parsed_email(&board.customer_email, "rep@example.com");
        let send = SendEmail::new(&parsed, None, Some(ReceivingEmail::To(board.user_id)))
            .with_company_id(Some(board.company_id));

        let affected = cancel_flow_on_inbound_email(&pool, &send).await.unwrap();
        assert_eq!(affected, 1);

        let row: (String,) = sqlx::query_as(
            "SELECT status FROM sms_flow_enrollments WHERE company_id = ? AND customer_id = ? LIMIT 1",
        )
        .bind(board.company_id)
        .bind(board.customer_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(row.0, "stopped_by_reply");
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn inbound_email_from_unknown_sender_does_not_cancel_flow(pool: MySqlPool) {
        let board = setup_board(&pool, Some("3173161456")).await;
        sqlx::query!(
            r#"
            INSERT INTO sms_flow_enrollments
                (flow_id, company_id, customer_phone_digits, customer_id, user_id, status, anchor_at)
            VALUES (1, ?, 5550000000, ?, 1, 'active', UTC_TIMESTAMP())
            "#,
            board.company_id,
            board.customer_id,
        )
        .execute(&pool)
        .await
        .unwrap();

        let parsed = parsed_email("stranger@example.com", "rep@example.com");
        let send = SendEmail::new(&parsed, None, Some(ReceivingEmail::To(board.user_id)))
            .with_company_id(Some(board.company_id));

        let affected = cancel_flow_on_inbound_email(&pool, &send).await.unwrap();
        assert_eq!(affected, 0);

        let row: (String,) = sqlx::query_as(
            "SELECT status FROM sms_flow_enrollments WHERE company_id = ? AND customer_id = ? LIMIT 1",
        )
        .bind(board.company_id)
        .bind(board.customer_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(row.0, "active");
    }
}
