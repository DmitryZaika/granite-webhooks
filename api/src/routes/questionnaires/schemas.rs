use crate::serde_helpers::js_date;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// A question with the deal's answer.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct AnsweredQuestion {
    /// Question id.
    pub id: i32,
    /// The question.
    pub question: String,
    /// The deal's answer; empty when not answered.
    pub answer: String,
}

/// A questionnaire with the deal's answers.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct DealQuestionnaire {
    /// Questionnaire id.
    pub id: i32,
    /// Name.
    pub name: String,
    /// Questions in creation order.
    pub questions: Vec<AnsweredQuestion>,
}

/// A question of a questionnaire.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct Question {
    /// Question id.
    pub id: i32,
    /// The question.
    pub question: String,
}

/// A questionnaire the caller made.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct OwnQuestionnaire {
    /// Questionnaire id.
    pub id: i32,
    /// Name.
    pub name: String,
    /// When it was made.
    #[serde(serialize_with = "js_date")]
    #[schema(value_type = Option<String>, format = DateTime)]
    pub created_at: Option<DateTime<Utc>>,
    /// Questions in creation order.
    pub questions: Vec<Question>,
}

/// Body of `POST /v1/questionnaires`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct NewQuestionnaire {
    /// Name, 1 to 255 characters.
    pub name: String,
    /// At least one question, each 1 to 5000 characters.
    pub questions: Vec<String>,
}

/// Body renaming a questionnaire.
#[derive(Debug, Deserialize, ToSchema)]
pub struct NameInput {
    /// New name, 1 to 255 characters.
    pub name: String,
}

/// Body of a question write.
#[derive(Debug, Deserialize, ToSchema)]
pub struct QuestionInput {
    /// The question, 1 to 5000 characters.
    pub question: String,
}

/// One answer.
#[derive(Debug, Deserialize, ToSchema)]
pub struct AnswerInput {
    /// Question id (of a questionnaire the caller can use).
    pub question_id: i32,
    /// Answer, up to 10000 characters; empty clears it.
    pub answer: String,
}

/// Body of `PUT /v1/deals/{id}/questionnaire-answers`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct AnswersInput {
    /// Answers to save; questions the caller cannot use are ignored.
    pub answers: Vec<AnswerInput>,
    /// Also add the answered questions to the deal as a note.
    #[serde(default)]
    pub create_note: bool,
}

/// Result of saving answers.
#[derive(Debug, Serialize, ToSchema)]
pub struct AnswersSaved {
    /// A note was added to the deal.
    pub note_created: bool,
}

/// The caller's default questionnaire.
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct DefaultQuestionnaire {
    /// Questionnaire id, or null when none is set.
    pub questionnaire_id: Option<i32>,
}
