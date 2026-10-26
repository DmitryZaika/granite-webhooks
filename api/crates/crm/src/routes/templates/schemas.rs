use crate::openapi::{IntoParams, ToSchema};
use crate::serde_helpers::js_date;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Who sees a template.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum TemplateScope {
    /// Everyone in the company (made by an admin).
    Company,
    /// Only its author.
    #[default]
    Personal,
}

/// A file sent with every email made from a template.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct TemplateAttachment {
    /// Attachment id.
    pub id: i32,
    /// File name.
    pub filename: String,
    /// File URL.
    pub url: String,
    /// MIME type, e.g. `image`.
    pub content_type: String,
    /// MIME subtype, e.g. `png`.
    pub content_subtype: String,
}

/// An email template as the composer's picker shows it.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct EmailTemplate {
    /// Template id.
    pub id: i32,
    /// Name in the picker.
    pub template_name: String,
    /// Subject (may hold `{{variables}}`).
    pub template_subject: String,
    /// Body HTML (may hold `{{variables}}`).
    pub template_body: String,
    /// Files sent with it.
    pub attachments: Vec<TemplateAttachment>,
    /// Company-wide or the caller's own.
    pub scope: TemplateScope,
    /// The caller may edit or delete it (only their own templates).
    pub can_edit: bool,
    /// Company templates: the deal list whose deals get it automatically.
    pub lead_list_id: Option<i32>,
    /// Hours after a deal enters `lead_list_id` that it is sent.
    pub hour_delay: Option<i32>,
    /// Name of that list (null when it was deleted).
    pub list_name: Option<String>,
    /// Name of the list's group.
    pub group_name: Option<String>,
    /// When it was made.
    #[serde(serialize_with = "js_date")]
    #[schema(value_type = Option<String>, format = DateTime)]
    pub created_at: Option<DateTime<Utc>>,
}

/// Body of `POST /v1/email-templates` and `PUT /v1/email-templates/{id}`.
#[derive(Debug, Clone, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct EmailTemplateInput {
    /// Name (1–255 characters).
    pub template_name: String,
    /// Subject (1–255 characters).
    pub template_subject: String,
    /// Body; `{{` and `}}` must pair up.
    pub template_body: String,
    /// Create only: `company` (admins) or `personal` (default).
    #[serde(default)]
    pub scope: Option<TemplateScope>,
    /// Company templates only: send it automatically when a deal enters
    /// this list (with `hour_delay`); null for no automation.
    #[serde(default)]
    pub lead_list_id: Option<i32>,
    /// Hours to wait (0–8760); required with `lead_list_id`.
    #[serde(default)]
    pub hour_delay: Option<i32>,
}

/// Body of `POST /v1/email-templates/{id}/attachments`.
#[derive(Debug, Clone, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct TemplateAttachmentInput {
    /// URL of a file uploaded to the `email-templates` folder.
    pub url: String,
    /// File name shown and sent.
    pub filename: String,
    /// MIME type, e.g. `application/pdf`.
    pub content_type: String,
}

/// Which template picker an order belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum OrderingType {
    /// SMS templates (CloudTalk and RingCentral share one list).
    Sms,
    /// Email templates.
    Email,
    /// Quote email templates.
    Quote,
}

impl OrderingType {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Sms => "sms",
            Self::Email => "email",
            Self::Quote => "quote",
        }
    }
}

/// How a picker sorts.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum OrderingMode {
    /// Most used first.
    #[default]
    Automatic,
    /// The order the user dragged.
    Custom,
}

/// Path of `/v1/me/template-order/{type}`.
#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Path)]
pub struct OrderPath {
    /// `sms`, `email` or `quote`.
    #[param(inline)]
    pub r#type: OrderingType,
}

/// The caller's sorting of one picker.
#[derive(Debug, Serialize, ToSchema)]
pub struct TemplateOrder {
    /// Automatic or custom.
    pub mode: OrderingMode,
}

/// Body of `PUT /v1/me/template-order/{type}`.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct SaveOrder {
    /// Every template of the picker, in the new order.
    pub template_ids: Vec<i32>,
}

/// Body of `POST /v1/templates/fill`.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct FillRequest {
    /// Text with `{{variables}}`.
    pub template: String,
    /// Deal the message is about.
    pub deal_id: Option<i32>,
    /// Customer the message is for.
    pub customer_id: Option<i32>,
    /// Without `customer_id`: the company's customer with this address.
    pub recipient_email: Option<String>,
}

/// Result of `POST /v1/templates/fill`.
#[derive(Debug, Serialize, ToSchema)]
pub struct Filled {
    /// The text with its variables filled.
    pub text: String,
}
