use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum DocumentEvent {
    DocumentCreated,
    DocumentSent,
    DocumentOpened,
    DocumentSigned,
    DocumentCompleted,
    DocumentRejected,
    DocumentCancelled,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct WebhookEvent {
    /// Тип события, которое вызвало вебхук
    pub event: DocumentEvent,

    /// Данные документа (вложенный объект)
    pub payload: Payload,

    /// Дата и время создания события вебхука
    #[serde(rename = "createdAt")]
    pub created_at: DateTime<Utc>,

    /// URL конечной точки, куда отправлен вебхук
    #[serde(rename = "webhookEndpoint")]
    pub webhook_endpoint: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Payload {
    /// ID документа
    pub id: u64,

    /// Внешний идентификатор (если есть)
    #[serde(rename = "externalId")]
    pub external_id: Option<String>,

    /// ID пользователя-владельца документа
    #[serde(rename = "userId")]
    pub user_id: u64,

    /// Опции аутентификации для документа (любая JSON-структура)
    #[serde(rename = "authOptions")]
    pub auth_options: Option<Value>,

    /// Значения полей формы (любой JSON)
    #[serde(rename = "formValues")]
    pub form_values: Option<Value>,

    /// Видимость документа (например, EVERYONE)
    pub visibility: String,

    /// Заголовок документа
    pub title: String,

    /// Текущий статус документа
    pub status: String,

    /// Идентификатор данных документа
    #[serde(rename = "documentDataId")]
    pub document_data_id: String,

    /// Дата и время создания документа
    #[serde(rename = "createdAt")]
    pub created_at: DateTime<Utc>,

    /// Дата и время последнего обновления документа
    #[serde(rename = "updatedAt")]
    pub updated_at: DateTime<Utc>,

    /// Дата и время завершения документа (если есть)
    #[serde(rename = "completedAt")]
    pub completed_at: Option<DateTime<Utc>>,

    /// Дата и время удаления документа (если есть)
    #[serde(rename = "deletedAt")]
    pub deleted_at: Option<DateTime<Utc>>,

    /// ID команды, если документ принадлежит команде
    #[serde(rename = "teamId")]
    pub team_id: Option<u64>,

    /// ID шаблона, если создан из шаблона
    #[serde(rename = "templateId")]
    pub template_id: Option<u64>,

    /// Источник документа (например, DOCUMENT, TEMPLATE)
    pub source: String,

    /// Метаданные документа (вложенный объект)
    #[serde(rename = "documentMeta")]
    pub document_meta: DocumentMeta,

    /// Массив получателей (в JSON ключ — "Recipient")
    #[serde(rename = "Recipient")]
    pub recipients: Vec<Recipient>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct DocumentMeta {
    /// ID метаданных документа
    pub id: String,

    /// Тема документа (если есть)
    pub subject: Option<String>,

    /// Сообщение, связанное с документом (если есть)
    pub message: Option<String>,

    /// Таймзона документа (например, "America/Indiana/Indianapolis")
    pub timezone: String,

    /// Пароль, если установлен (если есть)
    pub password: Option<String>,

    /// Формат даты в документе (например, "MM/DD/YYYY")
    #[serde(rename = "dateFormat")]
    pub date_format: String,

    /// URL для редиректа после подписания (если есть)
    #[serde(rename = "redirectUrl")]
    pub redirect_url: Option<String>,

    /// Порядок подписания (PARALLEL, SEQUENTIAL)
    #[serde(rename = "signingOrder")]
    pub signing_order: String,

    /// Разрешены ли печатные подписи (true/false)
    #[serde(rename = "typedSignatureEnabled")]
    pub typed_signature_enabled: bool,

    /// Язык документа (например, "en", "ru" и т.д.)
    pub language: String,

    /// Метод распространения документа
    #[serde(rename = "distributionMethod")]
    pub distribution_method: String,

    /// Настройки уведомлений по e-mail (любой JSON)
    #[serde(rename = "emailSettings")]
    pub email_settings: Option<Value>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Recipient {
    /// ID получателя
    pub id: u64,

    /// ID документа для этого получателя (если задан)
    #[serde(rename = "documentId")]
    pub document_id: Option<u64>,

    /// ID шаблона, если от шаблона
    #[serde(rename = "templateId")]
    pub template_id: Option<u64>,

    /// Электронная почта получателя
    pub email: String,

    /// Имя получателя
    pub name: String,

    /// Уникальный токен для этого получателя
    pub token: String,

    /// Дата/время удаления документа для этого получателя (если есть)
    #[serde(rename = "documentDeletedAt")]
    pub document_deleted_at: Option<DateTime<Utc>>,

    /// Дата/время истечения доступа (если есть)
    pub expired: Option<DateTime<Utc>>,

    /// Дата/время подписания документа (если есть)
    #[serde(rename = "signedAt")]
    pub signed_at: Option<DateTime<Utc>>,

    /// Опции аутентификации для этого получателя (любой JSON)
    #[serde(rename = "authOptions")]
    pub auth_options: Option<Value>,

    /// Порядок подписания этим получателем (если есть)
    #[serde(rename = "signingOrder")]
    pub signing_order: Option<u64>,

    /// Причина отказа, если получатель отклонил (если есть)
    #[serde(rename = "rejectionReason")]
    pub rejection_reason: Option<String>,

    /// Роль получателя (например, SIGNER, VIEWER)
    pub role: String,

    /// Статус прочтения документа этим получателем
    #[serde(rename = "readStatus")]
    pub read_status: String,

    /// Статус подписания этим получателем
    #[serde(rename = "signingStatus")]
    pub signing_status: String,

    /// Статус отправки (send status)
    #[serde(rename = "sendStatus")]
    pub send_status: String,
}
