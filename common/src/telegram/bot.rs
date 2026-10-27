//! The Telegram Bot API calls the apps make, and the parts of an update they
//! read. teloxide's types and their deserializers, compiled into each app
//! that sent a message, were half of time-triggered's code and a third of
//! webhooks'; these types hold only the fields the apps use.

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::Duration;

const TELEGRAM_API: &str = "https://api.telegram.org";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ChatId(pub i64);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct MessageId(pub i32);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct UserId(pub u64);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Chat {
    pub id: ChatId,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct User {
    pub id: UserId,
}

/// A message, sent or received. A callback's message older than 48 hours
/// comes without its text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Message {
    #[serde(rename = "message_id")]
    pub id: MessageId,
    pub chat: Chat,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<User>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}

impl Message {
    pub fn text(&self) -> Option<&str> {
        self.text.as_deref()
    }
}

/// A press on a callback button.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CallbackQuery {
    pub id: String,
    pub from: User,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<Message>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<String>,
}

/// What Telegram posts to the webhook; other kinds of update are `None` in both.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Update {
    pub update_id: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<Message>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub callback_query: Option<CallbackQuery>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum ParseMode {
    #[serde(rename = "HTML")]
    Html,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct InlineKeyboardMarkup {
    pub inline_keyboard: Vec<Vec<InlineKeyboardButton>>,
}

impl InlineKeyboardMarkup {
    pub fn new(
        rows: impl IntoIterator<Item = impl IntoIterator<Item = InlineKeyboardButton>>,
    ) -> Self {
        Self {
            inline_keyboard: rows
                .into_iter()
                .map(|row| row.into_iter().collect())
                .collect(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct InlineKeyboardButton {
    pub text: String,
    #[serde(flatten)]
    pub kind: InlineKeyboardButtonKind,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum InlineKeyboardButtonKind {
    Url(String),
    CallbackData(String),
}

impl InlineKeyboardButton {
    pub fn url(text: impl Into<String>, url: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            kind: InlineKeyboardButtonKind::Url(url.into()),
        }
    }

    pub fn callback(text: impl Into<String>, data: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            kind: InlineKeyboardButtonKind::CallbackData(data.into()),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum RequestError {
    /// Telegram refused the call (`ok: false`): its `error_code` and `description`.
    #[error("Telegram answered {code}: {description}")]
    Api { code: u16, description: String },
    #[error("Telegram could not be asked: {0}")]
    Network(String),
}

/// A bot: its token and the HTTP client, cheap to clone.
#[derive(Clone)]
pub struct Bot {
    /// `{api}/bot{token}`, which each method name follows.
    base: Arc<str>,
    http: reqwest::Client,
}

impl std::fmt::Debug for Bot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Bot")
    }
}

impl Bot {
    /// The bot with `token` on Telegram's API.
    pub fn new(token: impl AsRef<str>) -> Self {
        Self::with_api(token, TELEGRAM_API, None)
    }

    /// The bot of `TELOXIDE_TOKEN`, on `TELOXIDE_API_URL` and through
    /// `TELOXIDE_PROXY` when they are set, like teloxide's `Bot::from_env`.
    ///
    /// # Panics
    ///
    /// When `TELOXIDE_TOKEN` is not set.
    pub fn from_env() -> Self {
        let token = std::env::var("TELOXIDE_TOKEN").expect("TELOXIDE_TOKEN must be set");
        let api = std::env::var("TELOXIDE_API_URL").unwrap_or_else(|_| TELEGRAM_API.to_string());
        Self::with_api(token, &api, std::env::var("TELOXIDE_PROXY").ok().as_deref())
    }

    /// The bot with `token` on `api` (a stand-in in tests), with teloxide's
    /// timeouts.
    pub fn with_api(token: impl AsRef<str>, api: &str, proxy: Option<&str>) -> Self {
        let mut http = crate::http::client_builder()
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(17))
            .tcp_nodelay(true);
        if let Some(proxy) = proxy {
            http = http.proxy(reqwest::Proxy::all(proxy).expect("TELOXIDE_PROXY is a proxy URL"));
        }
        Self {
            base: format!("{}/bot{}", api.trim_end_matches('/'), token.as_ref()).into(),
            http: http.build().expect("HTTP client"),
        }
    }

    /// `sendMessage`.
    pub async fn send_message(
        &self,
        chat_id: ChatId,
        text: &str,
        reply_markup: Option<&InlineKeyboardMarkup>,
        parse_mode: Option<ParseMode>,
    ) -> Result<Message, RequestError> {
        #[derive(Serialize)]
        struct SendMessage<'a> {
            chat_id: ChatId,
            text: &'a str,
            #[serde(skip_serializing_if = "Option::is_none")]
            parse_mode: Option<ParseMode>,
            #[serde(skip_serializing_if = "Option::is_none")]
            reply_markup: Option<&'a InlineKeyboardMarkup>,
        }
        self.call(
            "sendMessage",
            &SendMessage {
                chat_id,
                text,
                parse_mode,
                reply_markup,
            },
        )
        .await
    }

    /// `editMessageText`; an empty `reply_markup` removes the buttons.
    pub async fn edit_message_text(
        &self,
        chat_id: ChatId,
        message_id: MessageId,
        text: &str,
        reply_markup: Option<&InlineKeyboardMarkup>,
    ) -> Result<Message, RequestError> {
        #[derive(Serialize)]
        struct EditMessageText<'a> {
            chat_id: ChatId,
            message_id: MessageId,
            text: &'a str,
            #[serde(skip_serializing_if = "Option::is_none")]
            reply_markup: Option<&'a InlineKeyboardMarkup>,
        }
        self.call(
            "editMessageText",
            &EditMessageText {
                chat_id,
                message_id,
                text,
                reply_markup,
            },
        )
        .await
    }

    /// `deleteMessage`.
    pub async fn delete_message(
        &self,
        chat_id: ChatId,
        message_id: MessageId,
    ) -> Result<(), RequestError> {
        #[derive(Serialize)]
        struct DeleteMessage {
            chat_id: ChatId,
            message_id: MessageId,
        }
        self.call::<bool>(
            "deleteMessage",
            &DeleteMessage {
                chat_id,
                message_id,
            },
        )
        .await
        .map(drop)
    }

    async fn call<R: DeserializeOwned>(
        &self,
        method: &str,
        request: &impl Serialize,
    ) -> Result<R, RequestError> {
        #[derive(Deserialize)]
        struct Answer<R> {
            ok: bool,
            result: Option<R>,
            error_code: Option<u16>,
            description: Option<String>,
        }
        let network =
            |error: reqwest::Error| RequestError::Network(error.without_url().to_string());
        let answer: Answer<R> = self
            .http
            .post(format!("{}/{method}", self.base))
            .json(request)
            .send()
            .await
            .map_err(network)?
            .json()
            .await
            .map_err(network)?;
        match answer.result {
            Some(result) if answer.ok => Ok(result),
            _ => Err(RequestError::Api {
                code: answer.error_code.unwrap_or_default(),
                description: answer.description.unwrap_or_default(),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn keyboards_serialize_as_the_bot_api_takes_them() {
        let keyboard = InlineKeyboardMarkup::new(vec![
            vec![
                InlineKeyboardButton::callback("Ann: 3", "assign:7:1"),
                InlineKeyboardButton::callback("Bob: 0", "assign:7:2"),
            ],
            vec![InlineKeyboardButton::url(
                "Open",
                "https://granite-manager.com/x",
            )],
        ]);
        assert_eq!(
            serde_json::to_value(&keyboard).unwrap(),
            json!({ "inline_keyboard": [
                [{ "text": "Ann: 3", "callback_data": "assign:7:1" },
                 { "text": "Bob: 0", "callback_data": "assign:7:2" }],
                [{ "text": "Open", "url": "https://granite-manager.com/x" }],
            ] })
        );
        assert_eq!(
            serde_json::to_value(InlineKeyboardMarkup::default()).unwrap(),
            json!({ "inline_keyboard": [] })
        );
        assert_eq!(
            serde_json::to_value(ParseMode::Html).unwrap(),
            json!("HTML")
        );
    }

    #[test]
    fn updates_keep_what_the_webhook_reads() {
        let message: Update = serde_json::from_value(json!({
            "update_id": 10,
            "message": {
                "message_id": 5, "date": 1_760_000_000, "text": "/email a@b.co",
                "chat": { "id": 42, "type": "private", "first_name": "Ann" },
                "from": { "id": 42, "is_bot": false, "first_name": "Ann" },
                "entities": [{ "type": "bot_command", "offset": 0, "length": 6 }],
            },
        }))
        .unwrap();
        let received = message.message.unwrap();
        assert_eq!(received.chat.id, ChatId(42));
        assert_eq!(received.text(), Some("/email a@b.co"));
        assert!(message.callback_query.is_none());

        let callback: Update = serde_json::from_value(json!({
            "update_id": 11,
            "callback_query": {
                "id": "77", "chat_instance": "-1", "data": "assign:7:2",
                "from": { "id": 9, "is_bot": false, "first_name": "Max" },
                "message": { "message_id": 6, "date": 0, "chat": { "id": 42, "type": "private" } },
            },
        }))
        .unwrap();
        let callback = callback.callback_query.unwrap();
        assert_eq!(callback.from.id, UserId(9));
        assert_eq!(callback.data.as_deref(), Some("assign:7:2"));
        let inaccessible = callback.message.unwrap();
        assert_eq!(inaccessible.id, MessageId(6));
        assert_eq!(inaccessible.text(), None);

        let other: Update = serde_json::from_value(
            json!({ "update_id": 12, "edited_message": { "message_id": 1 } }),
        )
        .unwrap();
        assert!(other.message.is_none() && other.callback_query.is_none());
    }

    #[tokio::test]
    async fn calls_post_json_and_read_the_answer() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let api = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let mut requests = Vec::new();
            for answer in [
                r#"{"ok":true,"result":{"message_id":3,"date":1,"chat":{"id":42,"type":"private"},"text":"hi"}}"#,
                r#"{"ok":false,"error_code":403,"description":"Forbidden: bot was blocked by the user"}"#,
                r#"{"ok":true,"result":true}"#,
            ] {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                let mut buffer = [0; 4096];
                loop {
                    let read = socket.read(&mut buffer).await.unwrap();
                    request.extend_from_slice(&buffer[..read]);
                    let text = String::from_utf8_lossy(&request);
                    if let Some(end) = text.find("\r\n\r\n") {
                        let length: usize = text
                            .lines()
                            .find_map(|line| {
                                line.to_ascii_lowercase()
                                    .strip_prefix("content-length: ")
                                    .map(str::to_string)
                            })
                            .and_then(|value| value.trim().parse().ok())
                            .unwrap_or(0);
                        if request.len() >= end + 4 + length {
                            break;
                        }
                    }
                }
                let reply = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{answer}",
                    answer.len()
                );
                socket.write_all(reply.as_bytes()).await.unwrap();
                requests.push(String::from_utf8_lossy(&request).into_owned());
            }
            requests
        });

        let bot = Bot::with_api("123:abc", &api, None);
        let keyboard =
            InlineKeyboardMarkup::new([[InlineKeyboardButton::url("Open", "https://x.test/")]]);
        let sent = bot
            .send_message(ChatId(42), "hi", Some(&keyboard), Some(ParseMode::Html))
            .await
            .unwrap();
        assert_eq!((sent.chat.id, sent.id), (ChatId(42), MessageId(3)));
        let refused = bot
            .edit_message_text(
                ChatId(42),
                MessageId(3),
                "edited",
                Some(&InlineKeyboardMarkup::default()),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(&refused, RequestError::Api { code: 403, description } if description.contains("blocked")),
            "{refused}"
        );
        bot.delete_message(ChatId(42), MessageId(3)).await.unwrap();

        let requests = server.await.unwrap();
        assert!(
            requests[0].starts_with("POST /bot123:abc/sendMessage HTTP/1.1"),
            "{}",
            requests[0]
        );
        assert!(requests[0].ends_with(
            r#"{"chat_id":42,"text":"hi","parse_mode":"HTML","reply_markup":{"inline_keyboard":[[{"text":"Open","url":"https://x.test/"}]]}}"#
        ));
        assert!(requests[1].starts_with("POST /bot123:abc/editMessageText "));
        assert!(requests[1].ends_with(
            r#"{"chat_id":42,"message_id":3,"text":"edited","reply_markup":{"inline_keyboard":[]}}"#
        ));
        assert!(requests[2].ends_with(r#"{"chat_id":42,"message_id":3}"#));
    }
}
