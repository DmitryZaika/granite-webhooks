use crate::axum_helpers::guards::Telegram;
use crate::libs::types::BasicResponse;
use common::telegram::bot::{
    Chat, ChatId, InlineKeyboardMarkup, Message, MessageId, ParseMode, RequestError, User, UserId,
};

use crate::libs::constants::ERR_SEND_TELEGRAM;
use crate::libs::constants::internal_error;
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{Arc, Mutex};

/// An email the code under test tried to send.
#[derive(Clone, Debug)]
pub struct SentEmail {
    pub to: Vec<String>,
    pub subject: String,
    pub body: String,
}

thread_local! {
    static SENT_EMAILS: std::cell::RefCell<Vec<SentEmail>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// Called instead of SES while tests run. Thread-local: each test has its own thread.
pub fn record_email(to: &[&str], subject: &str, body: &str) {
    SENT_EMAILS.with(|sent| {
        sent.borrow_mut().push(SentEmail {
            to: to.iter().map(|addr| (*addr).to_string()).collect(),
            subject: subject.to_string(),
            body: body.to_string(),
        });
    });
}

pub fn take_sent_emails() -> Vec<SentEmail> {
    SENT_EMAILS.with(|sent| std::mem::take(&mut *sent.borrow_mut()))
}

pub const fn telegram_user(id: u64) -> User {
    User { id: UserId(id) }
}

pub fn generate_message(chat_id: i64, text: &str) -> Message {
    generate_message_with_id(chat_id, 1, text)
}

pub fn generate_message_with_id(chat_id: i64, message_id: i32, text: &str) -> Message {
    Message {
        id: MessageId(message_id),
        chat: Chat {
            id: ChatId(chat_id),
        },
        from: Some(telegram_user(1)),
        text: Some(text.to_string()),
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
    async fn send_message<T>(&self, chat: ChatId, text: T) -> Result<Message, BasicResponse>
    where
        T: Into<String> + Send,
    {
        let text = text.into();
        let chat_id = chat.0;

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
    async fn send_repliable_message<T>(
        &self,
        chat: ChatId,
        text: T,
        repliable: InlineKeyboardMarkup,
        _parse_mode: Option<ParseMode>,
    ) -> Result<Message, RequestError>
    where
        T: Into<String> + Send,
    {
        let text = text.into();
        let chat_id = chat.0;
        self.sent
            .lock()
            .unwrap()
            .push((chat_id, text.clone(), Some(repliable)));
        if self.fail {
            Err(RequestError::Api {
                code: 403,
                description: "Forbidden: bot was blocked by the user".into(),
            })
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
