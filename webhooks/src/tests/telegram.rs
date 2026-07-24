use crate::axum_helpers::guards::Telegram;
use crate::libs::types::BasicResponse;
use chrono::Utc;
use teloxide::types::InlineKeyboardMarkup;
use teloxide::types::{Message, Recipient};

use crate::libs::constants::ERR_SEND_TELEGRAM;
use crate::libs::constants::internal_error;
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{Arc, Mutex};
use teloxide::types::{
    Chat, ChatId, ChatKind, ChatPrivate, EffectId, MediaKind, MediaText, MessageCommon,
    MessageKind, User, UserId,
};

pub fn telegram_user(id: u64) -> User {
    User {
        id: UserId(id),
        is_bot: false,
        first_name: "test".into(),
        last_name: None,
        username: None,
        language_code: None,
        is_premium: false,
        added_to_attachment_menu: false,
    }
}

pub fn generate_message_common(text: &str) -> MessageCommon {
    MessageCommon {
        author_signature: Some("Bot".to_string()),
        paid_star_count: Some(10),
        effect_id: Some(EffectId::default()),
        forward_origin: None,
        reply_to_message: None,
        external_reply: None,
        quote: None,
        reply_to_story: None,
        sender_boost_count: Some(5),
        edit_date: Some(Utc::now()),
        media_kind: MediaKind::Text(MediaText {
            text: text.to_string(),
            entities: Vec::new(),
            link_preview_options: None,
        }),
        reply_markup: None,
        is_automatic_forward: false,
        has_protected_content: false,
        is_from_offline: false,
        business_connection_id: None,
        reply_to_checklist_task_id: None,
    }
}

const fn get_chat(chat_id: i64) -> Chat {
    Chat {
        id: ChatId(chat_id),
        kind: ChatKind::Private(ChatPrivate {
            username: None,
            first_name: None,
            last_name: None,
        }),
    }
}

pub fn generate_message(chat_id: i64, text: &str) -> Message {
    generate_message_with_id(chat_id, 1, text)
}

pub fn generate_message_with_id(chat_id: i64, message_id: i32, text: &str) -> Message {
    let from_user = telegram_user(1);
    Message {
        id: teloxide::types::MessageId(message_id),
        thread_id: None,
        from: Some(from_user),
        sender_chat: None,
        date: Utc::now(),
        is_topic_message: false,
        via_bot: None,
        sender_business_bot: None,
        kind: MessageKind::Common(generate_message_common(text)),
        chat: get_chat(chat_id),
        is_paid_post: false,
        suggested_post_info: None,
        direct_messages_topic: None,
    }
}

type MockTelegramSent = Arc<Mutex<Vec<(i64, String, Option<InlineKeyboardMarkup>)>>>;
type MockTelegramDeleted = Arc<Mutex<Vec<(i64, i32)>>>;
type MockTelegramEdited = Arc<Mutex<Vec<(i64, i32, String)>>>;

#[derive(Clone)]
pub struct MockTelegram {
    pub sent: MockTelegramSent,
    pub deleted: MockTelegramDeleted,
    pub edited: MockTelegramEdited,
    pub fail: bool,
    pub fail_delete: bool,
    pub fail_edit_chat_ids: Arc<Mutex<Vec<i64>>>,
    next_message_id: Arc<AtomicI32>,
}

impl Default for MockTelegram {
    fn default() -> Self {
        Self {
            sent: Arc::new(Mutex::new(Vec::new())),
            deleted: Arc::new(Mutex::new(Vec::new())),
            edited: Arc::new(Mutex::new(Vec::new())),
            fail: false,
            fail_delete: false,
            fail_edit_chat_ids: Arc::new(Mutex::new(Vec::new())),
            next_message_id: Arc::new(AtomicI32::new(1)),
        }
    }
}

impl MockTelegram {
    pub fn new() -> Self {
        Self::default()
    }

    fn dummy_message(&self, chat_id: i64, text: &str) -> Message {
        let message_id = self.next_message_id.fetch_add(1, Ordering::SeqCst);
        generate_message_with_id(chat_id, message_id, text)
    }
}

impl Telegram for MockTelegram {
    async fn send_message<C, T>(&self, chat: C, text: T) -> Result<Message, BasicResponse>
    where
        C: Into<Recipient> + Send,
        T: Into<String> + Send,
    {
        let recipient = chat.into();
        let text = text.into();

        let chat_id = match recipient {
            Recipient::Id(id) => id.0,
            Recipient::ChannelUsername(_) => 0,
        };

        self.sent
            .lock()
            .unwrap()
            .push((chat_id, text.clone(), None));

        if self.fail {
            Err(internal_error(ERR_SEND_TELEGRAM))
        } else {
            Ok(self.dummy_message(chat_id, &text))
        }
    }
    async fn send_repliable_message<C, T>(
        &self,
        chat: C,
        text: T,
        repliable: InlineKeyboardMarkup,
    ) -> Result<Message, teloxide::RequestError>
    where
        C: Into<Recipient> + Send,
        T: Into<String> + Send,
    {
        let recipient = chat.into();
        let text = text.into();

        let chat_id = match recipient {
            Recipient::Id(id) => id.0,
            Recipient::ChannelUsername(_) => 0,
        };
        self.sent
            .lock()
            .unwrap()
            .push((chat_id, text.clone(), Some(repliable)));
        if self.fail {
            Err(teloxide::RequestError::Api(teloxide::ApiError::BotBlocked))
        } else {
            Ok(self.dummy_message(chat_id, &text))
        }
    }

    async fn edit_message_text<T>(
        &self,
        chat_id: i64,
        message_id: i32,
        text: T,
    ) -> Result<Message, BasicResponse>
    where
        T: Into<String> + Send,
    {
        let text = text.into();
        if self.fail_edit_chat_ids.lock().unwrap().contains(&chat_id) {
            return Err(internal_error(ERR_SEND_TELEGRAM));
        }
        self.edited
            .lock()
            .unwrap()
            .push((chat_id, message_id, text.clone()));
        Ok(generate_message_with_id(chat_id, message_id, &text))
    }

    async fn delete_message(&self, chat_id: i64, message_id: i32) -> Result<(), BasicResponse> {
        if self.fail_delete {
            return Err(internal_error(ERR_SEND_TELEGRAM));
        }
        self.deleted.lock().unwrap().push((chat_id, message_id));
        Ok(())
    }
}
