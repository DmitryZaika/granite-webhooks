use crate::axum_helpers::guards::Telegram;
use crate::libs::types::BasicResponse;
use chrono::Utc;
use teloxide::types::MaybeInaccessibleMessage;
use teloxide::types::{Message, Recipient};

use crate::libs::constants::ERR_SEND_TELEGRAM;
use crate::libs::constants::internal_error;
use std::sync::Mutex;
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
        effect_id: Some(EffectId::default()), // или ваш конструктор
        forward_origin: None,
        reply_to_message: None, // или Some(Box::new(...))
        external_reply: None,
        quote: None,
        reply_to_story: None,
        sender_boost_count: Some(5),
        edit_date: Some(Utc::now()),
        media_kind: MediaKind::Text(MediaText {
            text: text.to_string(),
            entities: Vec::new(),
            link_preview_options: None,
        }), // замените нужным вариантом
        reply_markup: None,
        is_automatic_forward: false,
        has_protected_content: false,
        is_from_offline: false,
        business_connection_id: None,
    }
}

fn get_chat(chat_id: i64) -> Chat {
    Chat {
        id: ChatId(chat_id),
        kind: ChatKind::Private(ChatPrivate {
            username: None,
            first_name: None,
            last_name: None,
        }),
    }
}

pub fn generate_message(chat_id: i64, text: String) -> Message {
    let from_user = telegram_user(1);
    Message {
        id: teloxide::types::MessageId(1),
        thread_id: None,
        from: Some(from_user),
        sender_chat: None,
        date: Utc::now(),
        is_topic_message: false,
        via_bot: None,
        sender_business_bot: None,
        kind: MessageKind::Common(generate_message_common(&text)),
        chat: get_chat(chat_id),
    }
}

#[derive(Default)]
pub struct MockTelegram {
    pub sent: Mutex<Vec<(i64, String)>>,
    pub fail: bool,
}

impl MockTelegram {
    pub fn new() -> Self {
        Self::default()
    }

    fn dummy_message(chat_id: i64, text: String) -> Message {
        generate_message(chat_id, text)
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
            Recipient::ChannelUsername(_) => 0, // для тестов можно забить
        };

        self.sent.lock().unwrap().push((chat_id, text.clone()));

        if self.fail {
            Err(internal_error(ERR_SEND_TELEGRAM))
        } else {
            Ok(Self::dummy_message(chat_id, text))
        }
    }

    async fn edit_message_text<T>(
        &self,
        _message: &MaybeInaccessibleMessage,
        text: T,
    ) -> Result<Message, BasicResponse>
    where
        T: Into<String> + Send,
    {
        // Если нужно — тоже логируешь
        let text = text.into();
        Ok(Self::dummy_message(0, text))
    }
}
