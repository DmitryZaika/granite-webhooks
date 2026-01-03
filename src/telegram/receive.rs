use crate::amazon::email::send_message;
use crate::axum_helpers::guards::{Telegram, TelegramBot};
use crate::crud::leads::assign_lead;
use crate::crud::leads::create_deal;
use crate::crud::users::{email_exists, get_user_tg_info, user_has_telegram_id};
use crate::crud::users::{get_user_telegram_token, set_telegram_id, set_user_telegram_token};
use crate::libs::constants::{ERR_DB, ERR_SEND_EMAIL, OK_RESPONSE};
use crate::libs::constants::{FORBIDDEN_RESPONSE, internal_error};
use crate::libs::types::BasicResponse;
use crate::telegram::utils::extract_message;
use crate::telegram::utils::parse_code;
use crate::telegram::utils::{gen_code, lead_url, parse_assign, parse_slash_email};
use axum::extract::State;
use axum::http::StatusCode;
use lambda_http::tracing;
use sqlx::MySqlPool;
use teloxide::prelude::*;
use teloxide::types::{ChatId, Update, UpdateKind};

const MESSAGE: &str = r"
Invalid message. Please send one of the following commands:
/email <email>
<code>
";

async fn handle_start_command<T: Telegram>(
    pool: &MySqlPool,
    bot: &T,
    email: &str,
    chat_id: ChatId,
) -> BasicResponse {
    let has_tg_id = match user_has_telegram_id(pool, chat_id.0).await {
        Ok(has_tg_id) => has_tg_id,
        Err(e) => {
            tracing::error!(?e, "Failed to check if user has telegram id");
            return internal_error(ERR_DB);
        }
    };
    if has_tg_id {
        return bot
            .send_message(chat_id, "You are already registered")
            .await
            .map_or_else(
                |e| e,
                |_| (StatusCode::OK, "User already has a telegram id"),
            );
    }
    let code = gen_code();
    let db_result = set_user_telegram_token(pool, chat_id.0, code, email).await;
    if db_result.is_err() {
        tracing::error!(
            ?db_result,
            chat_id = chat_id.0,
            email = email,
            "Failed to set user telegram token"
        );
        return internal_error(ERR_DB);
    }
    let message_result = send_message(
        &[email],
        "Graninte Manager Code",
        &format!("Your code is: {code}"),
    )
    .await;
    if let Err(e) = message_result {
        tracing::error!(?e, email = email, "email send failed");
        return internal_error(ERR_SEND_EMAIL);
    }

    let message =
        format!("You are now registering for {email}, please enter the code sent to your email");
    bot.send_message(chat_id, message)
        .await
        .map_or_else(|e| e, |_| OK_RESPONSE)
}

async fn handle_telegram_code<T: Telegram>(
    pool: &MySqlPool,
    bot: &T,
    chat_id: ChatId,
    code: i32,
) -> BasicResponse {
    let has_tg_id = match user_has_telegram_id(pool, chat_id.0).await {
        Ok(has_tg_id) => has_tg_id,
        Err(e) => {
            tracing::error!(
                ?e,
                chat_id = chat_id.0,
                "Failed to check if user has telegram id"
            );
            return internal_error(ERR_DB);
        }
    };
    if has_tg_id {
        return bot
            .send_message(chat_id, "You are already registered")
            .await
            .map_or_else(
                |e| e,
                |_| (StatusCode::OK, "User already has a telegram id"),
            );
    }
    let db_code = match get_user_telegram_token(pool, chat_id.0).await {
        Ok(Some(db_code)) => db_code,
        Ok(None) => {
            tracing::error!(
                chat_id = chat_id.0,
                "User does not have a confirmation token"
            );
            return (
                StatusCode::FORBIDDEN,
                "User does not have a confirmation token",
            );
        }
        Err(e) => {
            tracing::error!(?e, chat_id = chat_id.0, "Failed to get user telegram token");
            return internal_error(ERR_DB);
        }
    };
    if db_code == code {
        if let Err(e) = set_telegram_id(pool, chat_id.0).await {
            tracing::error!(?e, chat_id = chat_id.0, "Failed to set telegram id");
            return internal_error(ERR_DB);
        }
        return bot
            .send_message(chat_id, "Accepted, you are now registered")
            .await
            .map_or_else(|e| e, |_| OK_RESPONSE);
    }
    bot.send_message(chat_id, "Invalid code")
        .await
        .map_or_else(|e| e, |_| (StatusCode::OK, "Invalid code"))
}

async fn handle_message<T: Telegram>(msg: Message, pool: &MySqlPool, bot: &T) -> BasicResponse {
    let chat_id = msg.chat.id; // ChatId
    let Some(text) = msg.text() else {
        return OK_RESPONSE;
    };

    if text.starts_with("/start") {
        let full_message = "Welcome to our bot! Please send: /email <email>";
        return bot
            .send_message(chat_id, full_message)
            .await
            .map_or_else(|e| e, |_| (StatusCode::OK, "Invalid code"));
    }

    if let Some(email) = parse_slash_email(text) {
        match email_exists(pool, &email).await {
            Ok(true) => {
                return handle_start_command(pool, bot, &email, chat_id).await;
            }
            Ok(false) => {
                tracing::error!(email = email, "Email does not exist");
                return FORBIDDEN_RESPONSE;
            }
            Err(e) => {
                tracing::error!(?e, email = email, "Failed to check email existence");
                return internal_error(ERR_DB);
            }
        }
    }

    if let Some(code) = parse_code(text) {
        return handle_telegram_code(pool, bot, chat_id, code).await;
    }

    bot.send_message(chat_id, MESSAGE)
        .await
        .map_or_else(|e| e, |_| (StatusCode::OK, "Invalid code"))
}

async fn handle_assign_lead<T: Telegram>(
    pool: &MySqlPool,
    lead_id: i32,
    user_id: i64,
    bot: &T,
    cb: CallbackQuery,
) -> BasicResponse {
    let Some(message) = cb.message else {
        return (StatusCode::NOT_FOUND, "Invalid message");
    };
    let lead_result = assign_lead(pool, lead_id, user_id).await;
    if let Err(e) = lead_result {
        tracing::error!(
            ?e,
            lead_id = lead_id,
            user_id = user_id,
            "Failed to assign lead"
        );
        return internal_error(ERR_DB);
    }

    let tg_info = match get_user_tg_info(pool, user_id.try_into().unwrap()).await {
        Ok(Some(info)) => info,
        Ok(None) => {
            return (StatusCode::NOT_FOUND, "User not found");
        }
        Err(e) => {
            tracing::error!(?e, user_id = user_id, "Failed to get user info");
            return internal_error(ERR_DB);
        }
    };

    let former_message = extract_message(&message).unwrap_or_default();

    let user_name = tg_info.name.unwrap_or_else(|| "Unknown".to_string());
    let full_content = format!("{former_message}\n\nLead assigned to {user_name}");
    let edit_result = bot.edit_message_text(&message, full_content).await;
    if let Err(e) = edit_result {
        return e;
    }

    let result = match create_deal(pool, lead_id, 1, 0, user_id).await {
        Ok(deal) => deal,
        Err(e) => {
            tracing::error!(
                ?e,
                user_id = user_id,
                lead_id = lead_id,
                "Failed to create deal"
            );
            return internal_error(ERR_DB);
        }
    };
    let deal_id = result.last_insert_id();
    let lead_link = lead_url(deal_id);
    if let Some(telegram_id) = tg_info.telegram_id {
        let final_message = format!("You were assigned a lead. Click here: \n{lead_link}");
        return bot
            .send_message(ChatId(telegram_id), final_message)
            .await
            .map_or_else(|e| e, |_| (StatusCode::OK, "Invalid code"));
    }
    let bot_link = "https://t.me/granitemanager_bot?start";
    let message = format!(
        r"
    You were assigned a lead. Click here:
    {lead_link}

    Please link to telegram bot:
    {bot_link}

    Paste this command into the bot: \start {}
    ",
        tg_info.email
    );
    let message_result = send_message(&[&tg_info.email], "Lead assigned", &message).await;
    if message_result.is_err() {
        tracing::error!(
            ?message_result,
            email = tg_info.email,
            "Error sending email"
        );
        return internal_error(ERR_SEND_EMAIL);
    }

    OK_RESPONSE
}

async fn handle_callback<T: Telegram>(
    cb: CallbackQuery,
    pool: &MySqlPool,
    bot: &T,
) -> BasicResponse {
    let Some(data) = &cb.data else {
        return OK_RESPONSE;
    };
    if let Some((lead_id, user_id)) = parse_assign(data) {
        return handle_assign_lead(pool, lead_id, user_id, bot, cb).await;
    }
    OK_RESPONSE
}

pub async fn webhook_handler(
    State(pool): State<MySqlPool>,
    tg_bot: TelegramBot,
    axum::extract::Json(update): axum::extract::Json<Update>,
) -> BasicResponse {
    match update.kind {
        UpdateKind::Message(msg) => handle_message(msg, &pool, &tg_bot).await,
        UpdateKind::CallbackQuery(cb) => handle_callback(cb, &pool, &tg_bot).await,
        _ => OK_RESPONSE,
    }
}

#[cfg(test)]
mod local_tests {
    use super::*;
    use crate::tests::telegram::{MockTelegram, generate_message, telegram_user};
    use crate::tests::utils::insert_user;
    use axum::http::StatusCode;
    use sqlx::MySqlPool;
    use teloxide::types::CallbackQuery;

    fn chat_id(id: i64) -> ChatId {
        ChatId(id)
    }

    async fn create_default_user(pool: &MySqlPool, email: &str) -> Result<u64, sqlx::Error> {
        let rec = sqlx::query!(
            r#"
            INSERT INTO users (
                email,
                password,
                name,
                phone_number,
                is_employee,
                is_admin,
                is_superuser,
                is_deleted,
                company_id,
                telegram_id,
                telegram_conf_code,
                telegram_conf_expires_at,
                temp_telegram_id
            )
            VALUES (?, NULL, NULL, NULL, false, false, false, false, 1, NULL, NULL, NULL, NULL)
            "#,
            email
        )
        .execute(pool)
        .await?;

        Ok(rec.last_insert_id())
    }

    #[sqlx::test]
    fn test_message_invalid_option(pool: MySqlPool) {
        let message = generate_message(1, "Leeeroy Jenkins");
        let mock_bot = MockTelegram::new();
        handle_message(message, &pool, &mock_bot).await;

        let sent = mock_bot.sent.lock().unwrap();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].1, MESSAGE);
    }

    // -----------------------------
    // handle_start_command
    // -----------------------------

    #[sqlx::test]
    async fn test_start_already_registered(pool: MySqlPool) {
        let bot = MockTelegram::new();
        let user_id = create_default_user(&pool, "test@example.com")
            .await
            .unwrap();
        sqlx::query!("UPDATE users SET telegram_id = 123 WHERE id = ?", user_id)
            .execute(&pool)
            .await
            .unwrap();

        let res = handle_start_command(&pool, &bot, "test@example.com", chat_id(123)).await;

        assert_eq!(res.0, StatusCode::OK);

        let sent = bot.sent.lock().unwrap();
        assert_eq!(sent.len(), 1);
        assert!(sent[0].1.contains("already"));
    }

    #[sqlx::test]
    async fn test_start_db_error(pool: MySqlPool) {
        // force DB error: use nonexistent table or corrupted state
        let bot = MockTelegram::new();

        // переименуем таблицу
        sqlx::query!("RENAME TABLE users TO users_tmp")
            .execute(&pool)
            .await
            .unwrap();

        let res = handle_start_command(&pool, &bot, "a@b.com", chat_id(5)).await;

        assert_eq!(res.0, StatusCode::INTERNAL_SERVER_ERROR);

        sqlx::query!("RENAME TABLE users_tmp TO users")
            .execute(&pool)
            .await
            .unwrap();
    }

    #[sqlx::test]
    async fn test_start_sets_code_and_sends_email(pool: MySqlPool) {
        let bot = MockTelegram::new();

        create_default_user(&pool, "t@x.com").await.unwrap();
        let res = handle_start_command(&pool, &bot, "t@x.com", chat_id(7)).await;

        assert_eq!(res.0, StatusCode::OK);

        // сообщение отправлено
        let sent = bot.sent.lock().unwrap();
        assert_eq!(sent.len(), 1);
        assert!(sent[0].1.contains("please enter the code"));

        // код записан
        let tok = sqlx::query!("SELECT telegram_conf_code FROM users WHERE temp_telegram_id = 7")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert!(tok.telegram_conf_code.is_some());
    }

    // -----------------------------
    // handle_telegram_code
    // -----------------------------

    #[sqlx::test]
    async fn test_code_user_already_registered(pool: MySqlPool) {
        let bot = MockTelegram::new();

        let user_id = create_default_user(&pool, "test@example.com")
            .await
            .unwrap();
        sqlx::query!("UPDATE users SET telegram_id = 99 WHERE id = ?", user_id)
            .execute(&pool)
            .await
            .unwrap();
        let res = handle_telegram_code(&pool, &bot, chat_id(99), 111).await;

        assert_eq!(res.0, StatusCode::OK);
        let sent = bot.sent.lock().unwrap();
        assert_eq!(sent[0].1, "You are already registered");
    }

    #[sqlx::test]
    async fn test_code_no_token(pool: MySqlPool) {
        let bot = MockTelegram::new();

        let res = handle_telegram_code(&pool, &bot, chat_id(2), 123).await;

        assert_eq!(res.0, StatusCode::FORBIDDEN);
    }

    #[sqlx::test]
    async fn test_code_incorrect(pool: MySqlPool) {
        let bot = MockTelegram::new();

        let user_id = create_default_user(&pool, "test@example.com")
            .await
            .unwrap();
        sqlx::query!(
            "UPDATE users SET telegram_conf_code = 555, temp_telegram_id = 3 WHERE id = ?",
            user_id
        )
        .execute(&pool)
        .await
        .unwrap();
        let res = handle_telegram_code(&pool, &bot, chat_id(3), 111).await;

        assert_eq!(res.0, StatusCode::OK);

        let sent = bot.sent.lock().unwrap();
        assert!(sent[0].1.contains("Invalid code"));
    }

    #[sqlx::test]
    async fn test_code_success(pool: MySqlPool) {
        let bot = MockTelegram::new();

        let user_id = create_default_user(&pool, "test@example.com")
            .await
            .unwrap();
        sqlx::query!(
            "UPDATE users SET telegram_conf_code = 999, temp_telegram_id = 4 WHERE id = ?",
            user_id
        )
        .execute(&pool)
        .await
        .unwrap();

        let res = handle_telegram_code(&pool, &bot, chat_id(4), 999).await;

        assert_eq!(res.0, StatusCode::OK);

        let user = sqlx::query!("SELECT telegram_id FROM users WHERE id = ?", user_id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(user.telegram_id, Some(4));
        let sent = bot.sent.lock().unwrap();
        assert_eq!(sent[0].1, "Accepted, you are now registered");
    }

    // -----------------------------
    // handle_message
    // -----------------------------

    #[sqlx::test]
    async fn test_message_start(pool: MySqlPool) {
        let bot = MockTelegram::new();

        let msg = generate_message(1, "/start".into());
        let res = handle_message(msg, &pool, &bot).await;

        assert_eq!(res.0, StatusCode::OK);

        let sent = bot.sent.lock().unwrap();
        assert!(sent[0].1.contains("Welcome"));
    }

    #[sqlx::test]
    async fn test_message_email_valid(pool: MySqlPool) {
        let bot = MockTelegram::new();
        insert_user(&pool, "x@y.com", None).await.unwrap();
        let msg = generate_message(1, "/email x@y.com".into());

        let res = handle_message(msg, &pool, &bot).await;

        assert_eq!(res.0, StatusCode::OK);
    }

    #[sqlx::test]
    async fn test_message_email_invalid(pool: MySqlPool) {
        let bot = MockTelegram::new();
        let msg = generate_message(1, "/email x@y.com".into());

        let res = handle_message(msg, &pool, &bot).await;

        assert_eq!(res.0, StatusCode::FORBIDDEN);
    }

    #[sqlx::test]
    async fn test_message_invalid(pool: MySqlPool) {
        let bot = MockTelegram::new();
        let msg = generate_message(1, "whatever invalid".into());

        let res = handle_message(msg, &pool, &bot).await;

        assert_eq!(res.0, StatusCode::OK);

        let sent = bot.sent.lock().unwrap();
        assert!(sent[0].1.contains("Invalid message"));
    }

    // -----------------------------
    // handle_callback
    // -----------------------------

    #[sqlx::test]
    async fn test_callback_no_data(pool: MySqlPool) {
        let bot = MockTelegram::new();
        let cb = CallbackQuery {
            id: "a".into(),
            from: telegram_user(3),
            message: None,
            inline_message_id: None,
            chat_instance: "".into(),
            data: None,
            game_short_name: None,
        };

        let res = handle_callback(cb, &pool, &bot).await;

        assert_eq!(res.0, StatusCode::OK);
    }
}
